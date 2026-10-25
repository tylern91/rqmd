# CLI Reference

[← README](../README.md)

## Commands

| Command | Description |
|---------|-------------|
| `rqmd query <text> [--no-expand] [--timeout SECS [--timeout-strict]]` | Hybrid search: BM25 + vector + rerank + LLM query expansion. `--timeout` gives up after that many seconds (exit 124), or returns the results unreranked if only reranking is left; `--timeout-strict` always exits 124; see [Timeouts](#timeouts) |
| `rqmd search <text>` | BM25 keyword search only |
| `rqmd vsearch <text>` | Vector similarity only |
| `rqmd similar <path\|#docid>` | Find documents most similar to an already-indexed one |
| `rqmd get <path\|#docid>` | Retrieve document by path or content hash |
| `rqmd multi-get <glob>` | Retrieve multiple documents |
| `rqmd ls [collection[/path]]` | List collections or files |
| `rqmd embed [-c collection] [--rebuild] [--timeout SECS]` | Generate embeddings (`--rebuild`: clear vectors and re-embed from scratch). After an edit, a chunk whose text did not change reuses its stored vector, so only the edited chunks are encoded. A document that fails to embed is skipped and listed at the end; see [Exit codes](#exit-codes). `--timeout` stops after that many seconds with the work so far saved; see [Timeouts](#timeouts). `SIGINT` or `SIGTERM` stops it after the document in hand, with the work so far saved; see [Interrupting update and embed](#interrupting-update-and-embed) |
| `rqmd embed --cleanup` | Reclaim orphaned vectors and unreferenced content — no model, no re-embed. Vectors of an edited note that `rqmd embed` has not embedded yet are kept, because the next `embed` copies its unchanged chunks from them |
| `rqmd unlock [--force [--yes]]` | Remove the write lock left by a crashed or hung `embed`/`update`. Removes it only when the holder is provably dead; a live holder that has made no progress for `RQMD_LOCK_STALE_SECS` (default 600) needs `--force` and a confirmation. A holder that is alive and recently active is never removed |
| `rqmd update [-c collection] [--run-hooks] [--full]` | Re-index: reports new, updated, unchanged, and removed (soft-deleted) document counts. A file whose size and mtime match its last indexing is counted unchanged without being read; `--full` re-reads and re-hashes every file instead. `--run-hooks` runs each collection's `update_command` even when the index was picked up implicitly from a project-local `.rqmd/` — see [Update hook trust](SECURITY.md#update-hook-trust). The summary line ends with the folders the [built-in exclusions](CONFIGURATION.md#excluding-files) left out, when there are any, and a stored ignore pattern that no longer parses is reported as a warning. A hook that runs longer than `RQMD_HOOK_TIMEOUT_SECS` (default 300) is killed; a failed or timed-out hook never stops the other collections, and is listed at the end. `SIGINT` or `SIGTERM` stops it after the file in hand; see [Interrupting update and embed](#interrupting-update-and-embed) |
| `rqmd status` | Index health and collection summary |
| `rqmd doctor` | Diagnose config, index, model, and device issues |
| `rqmd bench [-n N]` | Embed throughput benchmark (default: 5 rounds) |
| `rqmd eval [--mode bm25\|vec\|hybrid] [--verbose]` | Search quality eval against synthetic fixtures |
| `rqmd mcp [--http] [--port N] [--host HOST] [--allow-non-loopback] [--daemon]` | Start MCP server |
| `rqmd mcp status` | Show MCP daemon status (pid, health, uptime) |
| `rqmd mcp stop` | Stop the running MCP daemon |
| `rqmd collection add <path> [--mask PATTERN] [--ignore PATTERN] [--hidden] [--respect-gitignore] [--max-file-bytes BYTES]` | Add a directory as a collection. The name must not be empty or contain `/`. Re-adding an existing name keeps its update hook and include/exclude setting; the mask, ignore, hidden, gitignore and size settings come from the new flags. A malformed `--mask` or `--ignore` glob fails the command before anything is stored. The summary names the folders the [built-in exclusions](CONFIGURATION.md#excluding-files) left out |
| `rqmd collection list` | List all collections |
| `rqmd collection remove <name>` | Remove a collection, its documents and its path contexts |
| `rqmd collection rename <old> <new>` | Rename a collection. The new name follows the same rules as `collection add`. Its documents are re-indexed for keyword search under the new name (no re-embed) and its path contexts move with it; full-text entries left under a name no collection uses (left by a rename made with an earlier release) are removed in the same pass. Waits for a running `embed` or `update`, like `collection remove` |
| `rqmd collection show <name>` | Show collection details, including its ignore patterns |
| `rqmd collection update-cmd <name> [cmd]` | Set/clear pre-update hook |
| `rqmd collection include/exclude <name>` | Toggle from default queries |
| `rqmd context add [path] <text>` | Add context for a path |
| `rqmd context list` | List all contexts |
| `rqmd context rm <path>` | Remove context |
| `rqmd context check` | Find paths missing context |
| `rqmd init` | Create a project-local `.rqmd` index |

Global flags (before the subcommand):

```
--index-dir <path>       Override index directory ($RQMD_INDEX_DIR)
--backend llama|ort      Inference backend ($RQMD_INFERENCE_BACKEND)
--ort-ep auto|coreml|cuda|directml|cpu   ORT execution provider ($RQMD_ORT_EP)
-v, --verbose            Show native model-loading and inference logs (also enabled by $RUST_LOG)
```

`rqmd mcp --host` (env `$RQMD_MCP_HOST`) accepts an IPv4 address, a hostname,
or an IPv6 literal, bare (`::1`) or bracketed (`[::1]`).

`rqmd mcp`'s `--allow-non-loopback` (env `$RQMD_MCP_ALLOW_NON_LOOPBACK`) is not
global — it only applies to the `mcp` subcommand, alongside a non-loopback
`--host`. See [MCP.md](MCP.md#binding-beyond-localhost) and
[SECURITY.md](../SECURITY.md) for what it gates.

---

## Query syntax and expansion

`rqmd query` (and the MCP `query` tool) auto-expand plain-text queries using a local
Qwen3-1.7B model, producing `lex`, `vec`, and `hyde` variants that are fused with the
original query via RRF. `rqmd search` and `rqmd vsearch` do **not** expand — they run
their respective single-mode search only.

The expansion prompt asks the model to write in the query's language. As a backstop, an
expanded line whose script family differs from the query's — Latin against Han/Kana/Hangul, in
either direction — is dropped, so a stray Japanese `lex:` line cannot replace an English
query's keywords. Two languages in the same script (French and English, say) cannot be told
apart this way and are not filtered.

**`rqmd query` flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--intent <text>` | *(none)* | Background context to steer expansion and reranking |
| `-n <num>` | `10` | Number of results to return |
| `-c/--collection <name>` | *(all)* | Scope to a collection (repeatable, OR-matched) |
| `--no-expand` | off | Skip LLM query expansion; search the query text as-is (env: `RQMD_NO_EXPAND`) |
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

**Equal scores:** results with exactly the same score are listed in ascending
`collection/path` order, so repeating a query returns the same order. This
applies to `query`, `vsearch` and `similar`.

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

## Colour and progress output

Escape sequences are written only to a terminal. Piped or redirected output, on stdout and on
stderr, is plain text. `NO_COLOR` (set to any value) turns styling off on a terminal, and
`TERM=dumb` turns it off and also stops live progress from redrawing a line in place.

| Environment | stdout / stderr is a terminal | Progress (`update`, `embed`, `collection add`) |
|-------------|-------------------------------|------------------------------------------------|
| default | colour | one line redrawn in place |
| `NO_COLOR` set | no colour | one line redrawn in place |
| `TERM=dumb` | no colour | a plain line each time a tenth of the work is done, each ending in a newline |
| not a terminal | no escapes | none |

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Error — the command did not complete |
| `2` | Completed with failures: `rqmd update` ran every collection but one or more update hooks failed or timed out, or `rqmd embed` finished but skipped documents it could not embed. Everything else was still indexed or embedded. (Command-line usage errors also exit `2`; they print usage text instead of a summary.) |
| `124` | Stopped by `--timeout` (the `timeout(1)` convention). `rqmd query` exits `124` only when the deadline passes before it has results, or with `--timeout-strict`; see [Timeouts](#timeouts) |
| `129`, `130`, `143` | `rqmd update` or `rqmd embed` stopped by `SIGHUP`, `SIGINT` or `SIGTERM` (`128` plus the signal number); see [Interrupting update and embed](#interrupting-update-and-embed) and [Update hooks and signals](#update-hooks-and-signals) |

`rqmd embed` stops early, with exit `1`, after five documents in a row fail to embed — that
pattern points at the model or GPU rather than at a document. Work embedded up to that point
is saved, so rerunning resumes.

On timeout a hook is sent `SIGKILL`. When stdin is not a terminal the hook runs in its own
process group and the whole group is killed; when stdin is a terminal the hook stays in the
foreground group (so Ctrl-C and credential prompts keep working) and only the hook's shell is
killed, so a background child it started can outlive it.

### Update hooks and signals

If `rqmd update` receives `SIGINT`, `SIGTERM` or `SIGHUP` while a hook is running, it forwards the
signal to the hook (to the whole process group when the hook has its own), waits up to 5 seconds
for the hook to exit, kills it if it has not, and exits with `128` plus the signal number (`130`,
`143`, `129`). The collections after the interrupted one are not updated. A terminal's Ctrl-C
already reaches a hook that shares rqmd's foreground group, so `SIGINT` is not sent to it a
second time. A signal that rqmd's parent had set to be ignored (for example `SIGHUP` under
`nohup`) stays ignored. A second signal while the hook is being stopped kills the hook at once
instead of waiting out the 5 seconds.

### Interrupting update and embed

`rqmd update` and `rqmd embed` stop at the next safe point when they receive `SIGINT` (Ctrl-C),
`SIGTERM` or `SIGHUP`. `rqmd update` finishes the file it is indexing, saves what it has indexed,
skips the removal pass and the rest of the collections, and exits. `rqmd embed` finishes the
document it is embedding, checkpoints the vectors, and exits. Both print a line saying that the
progress was saved and to rerun the command, and exit with `128` plus the signal number (`130`,
`143`, `129`). Rerunning continues where the run stopped: `update` skips the files it already
indexed (their size and mtime match), and `embed` embeds only the documents without vectors.

A second signal ends the process immediately with `128` plus its number, without finishing the
file or document in hand, without saving and without the hint, so it loses the same work as
killing the process: `embed` embeds the documents since its last checkpoint again on the next
run, and `update` may leave files it indexed in that run missing from search results. Waiting
for the index lock, loading a model and a single model call cannot be interrupted: the signal
is acted on once they return, or send a second one to leave at once.

## Rerank fallback

`rqmd query` reranks the fused candidates with the cross-encoder. If the rerank model fails, returns
an unusable score list (a different count than there are candidates, or no finite score), or the
active inference backend cannot rerank, the results keep their BM25 + vector fusion order and the
exit code stays `0`. Stderr gets one plain line, so stdout and `--format json` stay parseable and
the line reads the same under `NO_COLOR` and `TERM=dumb`:

```
rqmd: warning: results were not reranked: the rerank model failed, so they are in keyword and vector fusion order (set RUST_LOG=warn for details)
```

`RUST_LOG=warn` also logs the underlying error. `--no-rerank` is a request, not a failure, and
prints no warning. Running out of `--timeout` is a third cause, described under [Timeouts](#timeouts).

## Timeouts

`rqmd query --timeout SECS` and `rqmd embed --timeout SECS` stop with exit `124` once the
deadline has passed. The clock starts after the index is open, so opening the index and loading
models are not charged, and it is checked between stages: before embedding the query, before
query expansion and each expanded sub-query, and before reranking for `query`; before each
document for `embed`. A model call that is already running (one llama.cpp inference) cannot be
interrupted, so a run can overrun by the length of its longest single call.

Once `query` has its fused results, running out of time is not an error. If the deadline has
passed before reranking starts, or passes while it runs (it is checked between candidates, so
the candidate being scored finishes first), `query` skips the rest of the reranking, prints the
results in BM25 + vector fusion order, exits `0`, and writes one plain line to stderr (stdout and
`--format json` are unchanged):

```
rqmd: warning: results were not reranked: the time limit was reached, so they are in keyword and vector fusion order (--timeout of 5s; add --timeout-strict to exit 124 instead)
```

Add `--timeout-strict` (it requires `--timeout`) to keep exit `124` for a deadline that passes
before reranking starts; a rerank that is already running is not interrupted, as before. A
deadline that passes in an earlier stage (embedding the query, expansion) exits `124` either
way, because no results exist yet. `--no-rerank` has nothing left to skip, so it never prints this
line.

A timed-out `query` only reads, so the index is untouched. A timed-out `embed` checkpoints what it
has embedded first; rerun `rqmd embed` to resume. `rqmd search` and `rqmd vsearch` take no
`--timeout`: each is one index lookup with no stage between which to stop.

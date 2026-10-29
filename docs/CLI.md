# CLI Reference

[← README](../README.md)

## Commands

| Command | Description |
|---------|-------------|
| `rqmd query <text> [--no-expand] [--per-file N] [--recency HALF_LIFE] [--timeout SECS [--timeout-strict]] [--cache \| --no-cache]` | Hybrid search: BM25 + vector + rerank + LLM query expansion. `--timeout` gives up after that many seconds (exit 124), or returns the results unreranked if only reranking is left; `--timeout-strict` always exits 124; see [Timeouts](#timeouts). `--cache` reuses expansion output and rerank scores from earlier runs; off by default, see [Query cache](#query-cache) |
| `rqmd search <text> [--per-file N] [--recency HALF_LIFE]` | BM25 keyword search only |
| `rqmd vsearch <text> [--per-file N] [--recency HALF_LIFE]` | Vector similarity only; each result shows the chunk that matched |
| `rqmd similar <path\|#docid>` | Find documents most similar to an already-indexed one |
| `rqmd get <path\|#docid>` | Retrieve document by path or content hash |
| `rqmd multi-get <pattern> [-c collection]` | Retrieve multiple documents by a comma-separated list of paths, globs and `#docid`s; see [Multi-get patterns](#multi-get-patterns) |
| `rqmd ls [collection[/path]]` | List collections or files |
| `rqmd embed [-c collection] [--rebuild] [--timeout SECS]` | Generate embeddings (`--rebuild`: clear vectors and re-embed from scratch). A chunk whose text was already embedded under the same model, backend and context size reuses that stored vector, wherever it came from (an earlier version of the note or another document), so only new text is encoded. A document that fails to embed is skipped and listed at the end; see [Exit codes](#exit-codes). `--timeout` stops after that many seconds with the work so far saved; see [Timeouts](#timeouts). `SIGINT` or `SIGTERM` stops it after the document in hand, with the work so far saved; see [Interrupting update and embed](#interrupting-update-and-embed) |
| `rqmd embed --cleanup` | Reclaim orphaned vectors and unreferenced content — no model, no re-embed. Vectors of an edited note that `rqmd embed` has not embedded yet are kept, because the next `embed` copies its unchanged chunks from them. Also drops the vectors kept for documents excluded from embedding (`collection set --embed-ignore` / `--embed-max-bytes`), except vectors a non-excluded document with the same text still uses; when a collection has such a rule it prints `Swept N vector(s) kept for documents excluded from embedding.` |
| `rqmd embed --clear-cache` | Delete the [query cache](#query-cache) (`model_cache.sqlite`) and print how many rows it held. Loads no model, takes no index lock and leaves the index untouched; with no cache file it says so and exits `0` |
| `rqmd unlock [--force [--yes]]` | Remove the write lock left by a crashed or hung `embed`/`update`. Removes it only when the holder is provably dead; a live holder that has made no progress for `RQMD_LOCK_STALE_SECS` (default 600) needs `--force` and a confirmation. A holder that is alive and recently active is never removed |
| `rqmd update [-c collection] [--run-hooks] [--full]` | Re-index: reports new, updated, unchanged, and removed (soft-deleted) document counts. A file whose size and mtime match its last indexing is counted unchanged without being read; `--full` re-reads and re-hashes every file instead. `--run-hooks` runs each collection's `update_command` even when the index was picked up implicitly from a project-local `.rqmd/` — see [Update hook trust](SECURITY.md#update-hook-trust). The summary line ends with the folders the [built-in exclusions](CONFIGURATION.md#excluding-files) left out, when there are any, and a stored ignore pattern that no longer parses is reported as a warning. A hook that runs longer than `RQMD_HOOK_TIMEOUT_SECS` (default 300) is killed; a failed or timed-out hook never stops the other collections, and is listed at the end. `SIGINT` or `SIGTERM` stops it after the file in hand; see [Interrupting update and embed](#interrupting-update-and-embed) |
| `rqmd watch [-c collection] [--interval SECS]` | Keep the full-text index current while files change: checks the collections every `--interval` seconds (default 5, minimum 1) and re-indexes a collection whose files changed, with the same walk and rules as `rqmd update`. Keyword search only: it never embeds and never runs a collection's `update_command`. See [Watching for changes](#watching-for-changes) |
| `rqmd status` | Index health and collection summary. The AST Chunking block lists the extensions chunked at declaration boundaries and, after `Without:`, the indexed extensions that have no grammar with their document counts (prose such as `.md` and `.txt` is not listed). When a [query cache](#query-cache) file exists, the Documents block adds a `Cache:` line with its row count |
| `rqmd doctor` | Diagnose config, index, model, and device issues |
| `rqmd bench [-n N]` | Embed throughput benchmark (default: 5 rounds) |
| `rqmd eval [--mode bm25\|vec\|hybrid] [--verbose]` | Search quality gate: six built-in synthetic documents and 25 queries, scored by Hit@K in a throwaway index. Needs no index of yours and creates none |
| `rqmd eval --judgements FILE [--mode bm25\|vec\|hybrid] [--k K] [--json] [--no-rerank] [--no-expand] [--verbose]` | Score your own index against a judgement file: nDCG@10, MRR@10, Recall@K, Hit@K and per-query latency. Read-only. See [Evaluating search quality](#evaluating-search-quality) |
| `rqmd mcp [--http] [--port N] [--host HOST] [--allow-non-loopback] [--daemon] [--allow-refresh \| --no-refresh]` | Start MCP server. `--allow-refresh` lets `refresh: true` re-index an implicit project-local `.rqmd/` index; `--no-refresh` turns the option off for any index. See [Refreshing before a search](MCP.md#refreshing-before-a-search) |
| `rqmd mcp status` | Show MCP daemon status (pid, health, uptime) |
| `rqmd mcp stop [--force]` | Stop the running MCP daemon: `SIGTERM`, then a wait of up to 5 s. If the daemon is still running after that, `stop` exits `1`, says so and keeps the pidfile. `--force` then sends `SIGKILL`, and only after checking that the pid is still the recorded daemon: it must be alive with the same process start time as the pidfile records. A pid that started at another time (the pid was reused) is never signalled, and neither is a pidfile written by an older rqmd, which has no start time. See [Daemon lifecycle](MCP.md#daemon-lifecycle) |
| `rqmd collection add <path> [--mask PATTERN] [--ignore PATTERN] [--hidden] [--respect-gitignore] [--max-file-bytes BYTES]` | Add a directory as a collection. The name must not be empty or contain `/`. Re-adding an existing name keeps its update hook and include/exclude setting; the mask, ignore, hidden, gitignore and size settings come from the new flags. A malformed `--mask` or `--ignore` glob fails the command before anything is stored. The summary names the folders the [built-in exclusions](CONFIGURATION.md#excluding-files) left out |
| `rqmd collection list` | List all collections |
| `rqmd collection remove <name>` | Remove a collection, its documents and its path contexts |
| `rqmd collection rename <old> <new>` | Rename a collection. The new name follows the same rules as `collection add`. Its documents are re-indexed for keyword search under the new name (no re-embed) and its path contexts move with it; full-text entries left under a name no collection uses (left by a rename made with an earlier release) are removed in the same pass. Waits for a running `embed` or `update`, like `collection remove` |
| `rqmd collection show <name>` | Show collection details, including its ignore patterns and, when set, its `Embed ignore:` patterns and `Embed max:` cap |
| `rqmd collection set <name> [--embed-ignore PATTERN]... [--embed-max-bytes BYTES] [--clear-embed-ignore] [--clear-embed-max-bytes]` | Change a collection's embed exclusions after `collection add`. Matching documents stay in the full-text index and are never embedded. `--embed-ignore` (repeatable, same glob syntax as `--ignore`) replaces the stored patterns; `--embed-max-bytes` skips documents whose indexed text (not the raw file) is larger. Takes effect at the next `rqmd embed` and re-embeds nothing; vectors already stored for newly excluded documents are kept until `rqmd embed --cleanup`. `rqmd status` and `rqmd doctor` then report the excluded count. See [Excluding documents from embedding](CONFIGURATION.md#excluding-documents-from-embedding) |
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

With `--http` or `--daemon`, `$RQMD_MCP_IDLE_EXIT_SECS` (a positive whole number of seconds)
makes the server exit after that long without a request. See [MCP.md](MCP.md#idle-exit).

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
| `--per-file <N>` | `1` | Sections of one file to return, best first, up to 5; see [Sections per file](#sections-per-file) |
| `-c/--collection <name>` | *(all)* | Scope to a collection (repeatable, OR-matched) |
| `--recency <half-life>` | off | Rank newer files higher, for example `30d`, `2w` or `12h`; accepted by `query`, `search` and `vsearch`, see [Recency](#recency) |
| `--no-expand` | off | Skip LLM query expansion; search the query text as-is (env: `RQMD_NO_EXPAND`) |
| `--no-rerank` | off | Skip cross-encoder reranking (expansion still runs) |
| `--cache` | off | Reuse expansion output and rerank scores from earlier queries and store new ones; see [Query cache](#query-cache) (env: `RQMD_QUERY_CACHE`) |
| `--no-cache` | off | Turn the query cache off for this run, even when `--cache` or `RQMD_QUERY_CACHE` is set |
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

## Sections per file

By default a file is one result. `--per-file N`, accepted by `query`, `search`
and `vsearch`, returns up to `N` sections (chunks) of each file instead, so a
long note that answers a question in several places shows each place. `N` is
capped at 5 and `0` counts as 1; the default of 1 changes nothing.

- `-n` counts sections, not files: `-n 10 --per-file 3` can list as few as four
  files. `query` still ranks files first and only then picks sections, so it
  considers the same candidate files as without the flag.
- Sections are ordered like other results: by score, then by ascending
  `collection/path`. Sections of one file with equal scores are listed best
  match first, the later one first when they match equally.
- `query` and `search` choose sections by the words of the query: the best one
  as before, and further ones only if they hold one of the words in text the
  sections before them do not already show, so a word in the stretch two
  neighbouring chunks share counts once. A file with fewer matching sections
  than `N` yields fewer results. `query`
  scores each section with the reranker, so `N` extra sections cost up to `N`
  times the reranking of one; the extras are added only while one rerank pass
  stays within the 100-chunk pool ceiling, best-ranked files first.
- `vsearch` returns the `N` chunks of a file whose vectors are nearest the
  query. It always shows the chunk that matched, not the file's opening
  chunk, whatever `N` is.
- `search` with `--per-file 1` shows the file's opening chunk, as before.
- A file listed more than once is labelled `(section 2 of 3)` in `cli` output,
  `**section:** 2 of 3` in `md` and `section="2 of 3"` in `xml`, in words
  rather than colour. `json` and `csv` repeat the file with a different `line`
  and snippet, and `files` lists each file once.

## Recency

`--recency <half-life>`, accepted by `rqmd query`, `rqmd search` and `rqmd vsearch`,
ranks newer files higher. Each result's final score is multiplied by a factor of
the age of its file, taken from the file's modification time (mtime) as
`rqmd update` last recorded it:

```
factor = floor + (1 - floor) * 0.5 ^ (age / half-life)       floor = 0.5
```

A file modified just now keeps its whole score. The part of the score above the
floor halves every half-life, so with `--recency 30d` a file keeps 75% at 30
days, about 56% at 90 days and never less than 50%. Files are only down-weighted,
never removed from the results.

- The half-life is a whole number of days, weeks or hours (`30d`, `2w`, `12h`),
  above zero and at most `3650d`. Anything else is a usage error (exit 2) that
  shows the accepted form.
- Without the flag nothing changes: the same results, scores and order as
  before, whatever the file times are.
- Recency reweights the files the search already found, so a strongly relevant
  old file stays in the list. It does not pull in files the search did not match,
  and `-n` is still the number of results.
- `search` and `vsearch` normally cut their matches to `-n` before anything else,
  so a recent file just below the cut would never show. With `--recency` they
  first gather the best 4 x `-n` files (at most 200, and never fewer than `-n`),
  weight those by age, order them again and only then keep `-n`. A recent file
  that ranks below that pool is not found; raise `-n` to look further. `query`
  already ranks a wider candidate pool, which it weights in the same way.
- A file with no recorded mtime (not yet re-scanned by `rqmd update` since the
  mtime was first recorded, or modified within two seconds of being indexed)
  counts as new, as does an mtime in the future. Only the file's time is used,
  not dates inside the document. Tools that reset file times, such as a fresh
  `git clone` or a sync that does not preserve them, make every file look new.
- Equal scores are still ordered by ascending `collection/path`. With
  `--per-file`, every section of a file gets the file's factor.
- `rqmd query`, `rqmd search` and `rqmd vsearch` print one line to stderr, for
  example `rqmd: results are weighted by file age: half-life 30d, no file below
  50% of its score`. stdout and `--format` output are unchanged apart from the
  scores and their order.
- The scores `search` and `vsearch` print are the weighted ones, so with the flag
  they can be lower than without it.
- No index change and no rebuild: all three read the mtime the index already
  holds.

## Multi-get patterns

`rqmd multi-get <pattern>` (and the MCP `multi_get` tool) takes a comma-separated list of entries.
Spaces around each entry are ignored, and a leading `rqmd://` is dropped from path and glob entries.
Each entry is one of three kinds, decided by its form alone:

| Kind | Form | Matches | Example |
|------|------|---------|---------|
| docid | starts with `#` | the document whose content hash starts with the given hex; when several share the prefix, the first by `collection/path` | `#a1b2c3` |
| glob | contains `*`, `?`, `[` or `{` | every document whose `collection/path` matches; wildcards also match `/`, so `docs/*` covers `docs/a/b.md` | `notes/2025-05*.md` |
| path | anything else | every document whose path, or `collection/path`, equals the entry or ends with `/` followed by it | `guide/setup.md`, `README.md` |

How the entries combine:

- An entry's kind is fixed by its form; there is no fallback from one kind to another. `notes/*.md`
  is only ever a glob, never a literal path. A glob that does not parse fails the whole command, and
  `{a,b}` alternation cannot be used because `,` separates entries.
- No kind takes precedence over another. The result is the union of what every entry matched, and a
  document selected by several entries is returned once.
- A path entry returns all of its matches. An exact path does not hide suffix matches: `README.md`
  returns `README.md` and `guide/README.md` in every collection. Name the collection
  (`docs/README.md`) or a longer suffix to narrow it. The `/`-suffix part is case-insensitive for
  ASCII letters; the exact part is not.
- Documents are listed by `collection/path`, not in the order the entries were given.
- `-c/--collection` narrows path and glob entries. A docid entry is not narrowed: `#a1b2c3` returns
  its document even when it lives in a collection that `-c` leaves out.
- Long lists work: path entries are looked up in batches, so a pattern with thousands of entries
  returns the same documents as the same entries split over several calls. A repeated entry is
  looked up once, and a call with more than 5,000 distinct path entries fails with an error.

For example, with `docs/README.md`, `docs/guide/README.md` and `notes/todo.md` indexed:

```sh
rqmd multi-get 'notes/todo.md, README.md, docs/guide/*, #a1b2c3, missing.md'
```

returns `docs/README.md`, `docs/guide/README.md` and `notes/todo.md` in that order, each once,
whichever of them `#a1b2c3` names.

Entries that need a second look are reported on stderr, one line each, after the documents:

```
rqmd multi-get: no document matched "missing.md"
rqmd multi-get: "README.md" matched 2 documents; all are returned
```

A docid or glob entry is reported when it matches nothing, and a path entry also when it matches
more than one document. stdout, `--format` output and the exit code are unchanged: a pattern that
matches nothing still exits 0 and prints `No documents matched: <pattern>` to stderr.

## Evaluating search quality

`rqmd eval` with no `--judgements` is the built-in gate: six synthetic documents and 25 queries
indexed into a temporary index, scored by Hit@K against fixed thresholds. It ignores your index.
Its output and exit codes do not change.

`rqmd eval --judgements FILE` measures your own index instead. It opens the index read-only
(it never creates, updates or embeds anything; with no index it exits `1` as the other read
commands do), runs each judged query, and reports:

| Metric | Meaning |
|--------|---------|
| nDCG@10 | Graded gain `grade / log2(rank + 1)` over the top 10, divided by the same sum for the ideal order. The gain is the grade itself, not `2^grade - 1` |
| MRR@10 | Mean of `1 / rank` of the first relevant document in the top 10 (0 when none is) |
| Recall@K | Share of a query's relevant documents found in the top K, averaged over queries |
| Hit@K | Share of queries with at least one relevant document in the top K |
| Latency | p50, p95, max and mean milliseconds per query (nearest-rank percentiles) |

A document is relevant when its grade is 1 or more. nDCG and MRR always use 10; `--k` (default
10) sets the cutoff of Recall@K and Hit@K. Every number is the mean over the scored queries.

| Option | Effect |
|--------|--------|
| `--mode bm25\|vec\|hybrid` | Search path to score (default `bm25`). `vec` and `hybrid` need stored embeddings (`rqmd embed`) and a working inference backend, and exit `1` saying which is missing instead of scoring empty results. Here an unknown mode is an error |
| `--k K` | Cutoff for Recall@K and Hit@K |
| `--json` | One JSON document on stdout instead of the text report |
| `--no-rerank`, `--no-expand` | Hybrid only: skip the rerank stage or the query expansion, to isolate the fusion or to make runs repeatable |
| `--verbose` | Add a line per query to the text report |

All collections in the index are searched, including those excluded from default queries, so a
judged document is never out of reach.

### Judgement file

JSON, with one object per query. Documents are named `collection/path`, the collection name and
the path as `rqmd ls` shows them, which stay the same when the index is rebuilt (document ids and
docids do not).

```json
{
  "queries": [
    {
      "query": "how do I rotate the signing key",
      "relevant": {
        "runbooks/key-rotation.md": 3,
        "runbooks/release-checklist.md": 1,
        "notes/old-key-notes.md": 0
      }
    },
    { "query": "retry budget", "relevant": ["design/retries.md"] }
  ]
}
```

- `relevant` is an object mapping `collection/path` to an integer grade, or an array of
  `collection/path` strings (each grade 1). Grade 0 marks a document as judged and not relevant.
- Every query needs a non-empty `query` and at least one document graded 1 or more. No other
  fields are allowed, so a misspelled `relevant` is an error rather than an ignored query.
- A file ending in `.jsonl` holds the same query objects, one per line (blank lines are skipped),
  without the outer `{"queries": [...]}`.

A malformed file stops the run before the index is opened: nothing on stdout, the reason on
stderr (with the query number, or the line number for `.jsonl`), exit `1`.

A judged document that is not in the index (renamed, removed, or typed wrong) is not scored as a
miss. The report lists each one with its query, it is left out of that query's judgements, and
the run exits `2` after printing the scores of the rest. A query whose relevant documents are all
missing is listed as skipped and left out of the means; when no query can be scored the run
exits `1`.

`--json` output has these keys: `judgements`, `mode`, `k`, `queries_total`, `queries_scored`,
`queries_skipped` (`query`, `reason`), `metrics` (`ndcg_at_10`, `mrr_at_10`, `recall_at_k`,
`hit_at_k`), `latency_ms` (`p50`, `p95`, `max`, `mean`), `rerank_notice` (set when a hybrid run
was not reranked), `missing_documents` (`query`, `document`) and `per_query` (`query`, the
four metrics, `latency_ms`, and `ranked`, the retrieved `collection/path` keys). Compare two runs
by their `metrics`; the latency values vary from run to run. In `vec` and `hybrid` the first
query includes loading the model.

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

`rqmd query` reports its stages on stderr in the same way, never on stdout, in the order the query
reaches them (a stage repeated back to back shows once). The line is wiped before the results print:

| Label | Stage |
|-------|-------|
| `Searching keywords` | keyword (BM25) search |
| `Loading model / embedding query` | embedding the query for vector search; includes loading the embedding model on first use |
| `Loading model / expanding query` | query expansion; includes loading the generation model on first use |
| `Loading model / reranking 0/N`, then `Reranking 4/N`, `Reranking 8/N`, ... | reranking N candidates, which are scored four at a time so the count is live |

On a terminal the stages repaint one line. Under `TERM=dumb` each is a plain line ending in a
newline, and reranking prints a line each time a tenth of the candidates is done. When stderr is
not a terminal, `query` writes none of this, scores the candidates in one reranker call as before,
and its output is unchanged. Ranking and scores do not depend on whether the stages are shown.
`rqmd search` and `rqmd vsearch` have no stages to show.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Error — the command did not complete |
| `2` | Completed with failures: `rqmd update` ran every collection but one or more update hooks failed or timed out, or `rqmd embed` finished but skipped documents it could not embed, or `rqmd eval --judgements` scored the index but some judged documents are missing from it. Everything else was still indexed, embedded or scored. (Command-line usage errors also exit `2`; they print usage text instead of a summary.) |
| `124` | Stopped by `--timeout` (the `timeout(1)` convention). `rqmd query` exits `124` only when the deadline passes before it has results, or with `--timeout-strict`; see [Timeouts](#timeouts) |
| `129`, `130`, `143` | `rqmd update`, `rqmd embed` or `rqmd watch` stopped by `SIGHUP`, `SIGINT` or `SIGTERM` (`128` plus the signal number); see [Interrupting update and embed](#interrupting-update-and-embed) and [Update hooks and signals](#update-hooks-and-signals) |

With no index at the resolved directory (no `index.sqlite` there), commands that only read
create nothing and print `` no index at <dir>; run `rqmd collection add <path>` to create one ``.
`rqmd search`, `vsearch`, `query`, `get` and `multi-get` print it on stderr and exit `1`;
`rqmd status`, `collection list`, `context list` and `ls` print it on stdout and exit `0`, and
`rqmd doctor` reports `SQLite exists: NO` with the same hint and exits `0`. `rqmd collection add`,
`update`, `embed` and `init` create the index.

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

### Watching for changes

`rqmd watch` is a long-running command for keeping keyword search current without running
`rqmd update` by hand. Every `--interval` seconds (default 5, minimum 1) it walks each collection
and compares the files' sizes and modification times with what it saw on the previous check. It
opens the index only for a collection whose files differ, and re-indexes that collection the way
`rqmd update` does, so the mask, ignore patterns, hidden-file, `.gitignore`, size-limit and symlink
rules are the same. New files become searchable, edited files are replaced, and deleted files are
removed. `-c` limits it to one collection; a collection added or removed while it runs is picked up
on the next check. It polls, so a change shows up within about one interval plus the time the pass
takes. A file written within the last couple of seconds is looked at again on the next check.

- **Keyword search only.** It loads no model and never embeds. Run `rqmd embed` when you want
  vectors for what changed.
- **Hooks never run.** A collection's `update_command` is not run by `rqmd watch`, whatever the
  index's origin, because a command that fires on every file change is not what the hook was set up
  for.
- **The lock is held only while a pass runs.** `rqmd update`, `rqmd embed` and
  `rqmd collection` work while a watch is idle. When another process holds the index lock, watch
  prints `waiting for the index lock` once and tries again, waiting longer each time (up to 30
  seconds), instead of failing.
- **Paused during a rebuild.** While the full-text index still needs its one-time rebuild (an
  index from an older release, or a new one nothing has run `rqmd update` on yet), watch prints
  that it is paused and indexes nothing. It does not take the lock then, so the `rqmd update` that
  performs the rebuild is not held up; watch resumes by itself afterwards.
- **Stopping.** `SIGINT` (Ctrl-C) or `SIGTERM` stops it after the file in hand, releases the lock
  and exits `130` or `143`. A second signal exits immediately.

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

## Query cache

`rqmd query --cache` (or `RQMD_QUERY_CACHE=1`) keeps the query-expansion output and the rerank
scores of each query in `model_cache.sqlite` in the index directory, and reuses them when the same
query runs again: a repeated query calls neither the generation model nor the reranker, and
returns the same results. The cache is off by default; without `--cache`, `query` creates no file
and behaves exactly as before. `--no-cache` turns it off for one run when the environment variable
is set.

- **What is reused.** An expansion is reused for the same query text, intent, generation model and
  prompt version; a rerank score for the same query (with its intent), the same chunk text and the
  same rerank model and prompt version. A model is identified by its name and by the size and
  modification time of its weights file, so replacing the weights under the same name misses. An
  edited document has new chunk text, so only its chunks are scored again. Upgrading rqmd misses
  the whole cache once.
- **What is never stored.** An expansion that failed or was skipped, the scores of a rerank that
  failed, ran out of `--timeout`, was skipped (`--no-rerank`) or was not supported, and any
  non-finite score. Results in fusion order are never cached.
- **Size.** At most 10,000 rows, expansions and rerank scores counted together. A write that goes
  over evicts the least recently used rows. `rqmd update` does not clear it: rows for text that no
  longer exists are simply never looked up again, and age out.
- **Clearing.** `rqmd embed --clear-cache` deletes the file. Deleting `model_cache.sqlite` by hand
  is equally safe. `rqmd embed --cleanup` does not touch it.
- **Failures.** A cache file that cannot be created or read never fails the query: it prints one
  line to stderr and runs without the cache. A cache another `rqmd` process is writing to is
  skipped for that run without a message.
- **Privacy.** The file holds query-derived text (the expanded queries the model wrote) and
  relevance scores on disk, beside the index. Query text and chunk text are stored only as SHA-256
  digests, but an expansion restates the query. The file is created readable by its owner only
  (`0600` on Unix).
- **Scope.** Only `rqmd query` uses it. The MCP server, `rqmd eval` and `rqmd bench` do not.

`RUST_LOG=rqmd::query_cache=debug` logs, per query, whether the expansion came from the cache or
the model and how many chunks the reranker scored.

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
deadline has passed. The clock starts after the index is open, so opening the index and downloading
models are not charged (loading a model into memory happens inside the stage that first needs it,
and is charged), and it is checked between stages: before embedding the query, before
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

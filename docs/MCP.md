# MCP Server

[← README](../README.md)

rqmd includes a built-in MCP server exposing its search index as tools for Claude, Cursor, and other MCP-aware clients.

| Tool | Description |
|------|-------------|
| `query` | Hybrid search: BM25 + vector + rerank + LLM expansion (recommended) |
| `search` | BM25 keyword search — no models required |
| `get` | Retrieve a document by path or content hash |
| `multi_get` | Retrieve multiple documents by glob pattern |
| `status` | Index health and collection summary |

All five tools are lookups and declare `readOnlyHint: true` in their tool annotations, so a client
can treat them as read-only (for example, run them in plan mode without a prompt). The one
exception to a pure lookup is the opt-in `refresh: true` on `search` and `query`, which updates the
full-text index before searching (see [Refreshing before a search](#refreshing-before-a-search));
the index is a cache derived from the collection files, and a refresh never writes to the files. `openWorldHint`
is left unset on every tool: `query` may download its models on first use, so a closed-world claim
would be false for it. `destructiveHint` and `idempotentHint` are not set, because both are
meaningful only for tools that are not read-only.

```sh
rqmd mcp                        # stdio (Claude Desktop, Cursor, etc.)
rqmd mcp --http                 # Streamable HTTP on port 8181
rqmd mcp --http --port 9000     # custom port
rqmd mcp --http --host 0.0.0.0  # bind on all interfaces — see warning below
rqmd mcp --daemon               # background HTTP (implies --http)
rqmd mcp status                 # pid, health, uptime of the running daemon
rqmd mcp stop                   # stop the running daemon
rqmd mcp stop --force           # ...and SIGKILL it if it ignores SIGTERM
```

For Claude Desktop, add to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rqmd": {
      "command": "rqmd",
      "args": ["mcp"]
    }
  }
}
```

## Daemon lifecycle

`rqmd mcp --daemon` forks the HTTP server into the background and tracks it
under the index directory: a pidfile at `<index-dir>/mcp.pid` and its
stdout/stderr log at `<index-dir>/mcp.log`. `rqmd mcp status` and
`rqmd mcp stop` don't just trust the pidfile — before sending a stop signal
or reporting the daemon as running, they issue a `GET /health` request on the
recorded host:port and cross-check the pid the daemon reports against the
pid on record. Only an exact match counts as confirmed; an unreachable
`/health` means the pidfile is stale, and a reachable `/health` reporting a
*different* pid means another process now owns that port. Starting a daemon
on a port that's already bound fails immediately with an error instead of
silently colliding with the existing listener.

The pidfile also records the daemon's own process start time. A daemon that is alive
but not answering `/health` (for example one that is slow to shut down) is therefore
told apart from a stale pidfile: `rqmd mcp status` reports it as running but not
answering, and `rqmd mcp stop` sends it `SIGTERM` and keeps the pidfile if it is still
there after 5 seconds, exiting `1` and saying the process is still running.

A pidfile that names pid 0 or 1, a number above `i32::MAX` or the `rqmd` process itself is
ignored as if it were absent. A `/health` answer from a host that is not loopback is
trusted only when the process start time in the pidfile matches too.

A pidfile with no start time (written by an older rqmd, or on a Unix other than macOS and
Linux), or one whose pid the operating system will not report on (for example a process
owned by another user), cannot be told apart from a recycled pid. `rqmd mcp status`, `rqmd mcp stop` (with or without `--force`)
and `rqmd mcp --daemon` then say the process is running but cannot be checked, keep the
pidfile, and signal nothing. If it is the daemon, stop it yourself; if not, delete the
pidfile (the message names it).

`rqmd mcp stop --force` goes one step further. After the same `SIGTERM` and wait, if
the process is still alive it sends `SIGKILL` and removes the pidfile, but only once it
has checked that the pid is still the recorded daemon, meaning that it is alive and its
start time matches the pidfile's. It refuses, and says why, when the pid started at a
different time (the pid was reused by another process), when the process start time
cannot be read, or when the pidfile has no start time because an older rqmd wrote it;
in the last two cases the pidfile is kept. `--force` is never implied: without it
`SIGKILL` is never sent. Unix only.

On SIGTERM or Ctrl-C the server ends every open event stream before it exits, so a
connected client that holds a `GET /mcp` stream open no longer delays `rqmd mcp stop`.
Tool calls answer over the same kind of stream, so a call that has not returned yet is
cut off as well and the client gets no result for it.

## Idle exit

Set `RQMD_MCP_IDLE_EXIT_SECS` to a positive whole number of seconds and `rqmd mcp --http` (or
`--daemon`) exits once it has gone that long without a request. It is off unless set. `0` or a
value that is not a whole number is an error, reported before the server starts (for `--daemon`,
before it is launched).

Only requests to `/mcp` count as activity. A client that keeps an event stream (`GET /mcp`) open
without calling anything does not keep the server up, and neither do `/health` or
`/health/daemon` requests, so `rqmd mcp status` can be run at any time without postponing the exit.
A request that is still being answered, such as a long `query`, keeps the server up until its
response has been sent; the idle period starts again after that.

When the period passes, the server writes `[rqmd-mcp] idle for <N>s; shutting down` to its log,
ends open event streams as it does on SIGTERM, removes its pidfile and exits with status 0. A
client that connects afterwards gets connection refused until the daemon is started again.

## Binding beyond localhost

`--host` (default `127.0.0.1`, env `RQMD_MCP_HOST`) controls the bind
address for `--http`/`--daemon` mode; `--port` (default `8181`, env
`RQMD_MCP_PORT`) controls the port. `127.0.0.1`, `localhost`, and `::1` count
as loopback; anything else is non-loopback.

`--host` takes an IPv4 address, a hostname, or an IPv6 literal. IPv6 may be
given bare (`--host ::1`) or bracketed (`--host '[::1]'`); rqmd binds the
address and accepts the bracketed `Host: [::1]:<port>` form in requests. Use
`--host ::1` for IPv6 loopback — it is not served by `127.0.0.1`.

The server also speaks cleartext HTTP/2 to clients that use it with prior knowledge
(`curl --http2-prior-knowledge http://127.0.0.1:8181/health`). There is no TLS and no ALPN, so
clients that negotiate HTTP/2 over `https://` cannot connect; plain `http://` clients stay on
HTTP/1.1. The host check reads the `Host` header, or the `:authority` of an HTTP/2 request, and
answers 403 when both are present and differ.

Passing a non-loopback `--host` to `--http`/`--daemon` **refuses to start**
with an error, not just a warning:

> refusing to bind the MCP server to non-loopback host {host}: this exposes
> the index's full-text and semantic search — including `get`, which returns
> arbitrary indexed file content — with no authentication to anything that
> can reach {host}:{port}.
>
> If this is intentional (e.g. a trusted network or container), pass
> `--allow-non-loopback` (or set `RQMD_MCP_ALLOW_NON_LOOPBACK=1`).

rqmd ships **no authentication** for the HTTP/MCP listener at all — anyone
who can reach the bound host:port can query and read every indexed
document. Treat `--host 0.0.0.0` (or any other non-loopback address) plus
`--allow-non-loopback` as production-network-exposure, not a convenience
flag. See [SECURITY.md](../SECURITY.md) for the full security posture.

## Concurrency

`search`, `get`, `multi_get` and `status` are served from a small pool of
read-only store handles (`RQMD_MCP_FTS_READERS`, default `min(4, cores)`), so
concurrent clients overlap instead of queueing. Every tool call runs on the
blocking thread pool, so a slow call cannot stall other requests or the
`/health` endpoint.

`query` still runs one call at a time: the embedding, rerank and generation
models are held once and need exclusive access, and giving each request its own
copy would multiply a multi-gigabyte footprint. Concurrent `query` calls wait
for each other; they no longer delay `search`, `get` or `status`.

A `query` call is also bounded by `RQMD_MCP_TOOL_TIMEOUT_SECS` (default 120): once the call
holds the model store, it returns a `timed out after …` error as soon as the deadline has
passed at the next stage boundary (before expansion, each expanded sub-query, or embedding the
query). A model call already running is not interrupted, so a call can overrun by one stage. The
other tools are single index lookups and have no such bound. A refresh pass has its own bound of the
same length (see [Refreshing before a search](#refreshing-before-a-search)).

Once `query` has its fused results, the deadline no longer makes it fail: if it passes before
reranking starts, or while candidates are being reranked, `query` returns the results in fusion
order and ends the text with one `Note:` line saying they were not reranked. Pass
`strict_timeout: true` to get the `timed out after …` error instead for a deadline that passes
before reranking starts; a rerank already running is not interrupted.

If the rerank model fails, returns an unusable score list, or the active backend cannot rerank,
`query` still returns its results in fusion order and ends the text with one `Note:` line saying
the results were not reranked. The note names no file path and does not repeat the error; the
daemon's log (`RUST_LOG=warn`) has it. Passing `rerank: false` is a request and adds no note.

## MCP tool parameters

Exact input fields per tool, as accepted by the JSON-RPC tool call
(`collections` is plural, matching the CLI's repeatable `-c`/`--collection`):

| Tool | Field | Type | Notes |
|------|-------|------|-------|
| `query` | `query` | `string` (required) | The search text |
| | `intent` | `string`, optional | Background context steering expansion/reranking |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `limit` | `number`, optional | Default 10 |
| | `offset` | `number`, optional | Results to skip, for the next page. Default 0, capped at 1000 |
| | `per_file` | `number`, optional | Sections of one file to return, best first. Default 1, capped at 5 (see below) |
| | `recency` | `string`, optional | Half-life such as `30d`, `2w` or `12h`: rank newer files higher (see below). Default: off |
| | `refresh` | `boolean`, optional | `true`: bring the full-text index up to date with the files before searching (see [Refreshing before a search](#refreshing-before-a-search)). Default `false` |
| | `rerank` | `boolean`, optional | Default `true` |
| | `expand` | `boolean`, optional | Default `true` |
| | `strict_timeout` | `boolean`, optional | Default `false`: a tool timeout that passes before reranking returns the results in fusion order with a note. `true`: it returns a `timed out` error instead |
| `search` | `query` | `string` (required) | BM25-only search text |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `limit` | `number`, optional | Default 10 |
| | `offset` | `number`, optional | Results to skip, for the next page. Default 0, capped at 1000 |
| | `per_file` | `number`, optional | Sections of one file to return, best first. Default 1, capped at 5 (see below) |
| | `recency` | `string`, optional | Half-life such as `30d`, `2w` or `12h`: rank newer files higher (see below). Default: off |
| | `refresh` | `boolean`, optional | `true`: bring the full-text index up to date with the files before searching (see [Refreshing before a search](#refreshing-before-a-search)). Default `false` |
| `get` | `file` | `string` (required) | Path or `#docid` |
| | `from_line` | `number`, optional | Start line for partial retrieval |
| | `max_lines` | `number`, optional | Cap on lines returned (default 2000) |
| `multi_get` | `pattern` | `string` (required) | Comma-separated paths, globs and `#docid`s (see [multi_get patterns](#multi_get-patterns)) |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `max_lines` | `number`, optional | Cap on lines returned per document (default 2000) |
| `status` | *(none)* | | Takes no input. Reports document/vector counts, `Pending embed: N` (documents whose text has no current embedding, counted for the backend the server runs with, so it can differ from a CLI run under another `RQMD_INFERENCE_BACKEND`) and per-collection document counts; it does not disclose filesystem paths |

`query` and `search` page with `offset`: a response that is followed by more
results ends with `More results are available: pass offset=N.`, and an
`offset` past the last result returns an empty page without an error. `search`
pages are slices of one BM25 ranking. `query` sizes its rerank candidate pool
from the number of results it must return (`offset + limit`), so a deeper page
is ranked over a slightly larger pool and a result near a page boundary can
shift by a position.
Results with exactly the same score are ordered by ascending `collection/path`,
so a tie at a page boundary falls the same way on every request.

`per_file` makes `query` and `search` return up to that many sections of one
file, each as its own result with its own snippet, labelled `section n of m`
when a file appears more than once. The server caps it at 5 whatever the client
sends, and `0` counts as 1. `limit` and `offset` count sections, so a page can
hold fewer files. Omitted, it is 1: one result per file, as before. Sections
are chosen as for the CLI's `--per-file`; see
[Sections per file](CLI.md#sections-per-file).

`recency` makes `query` and `search` multiply each result's score by a factor
of its file's modification time that falls from 1 toward 0.5, with the part
above 0.5 halving every half-life. Files are only down-weighted, never removed.
The value is a whole number of days, weeks or hours, above zero and at most
`3650d`; anything else returns an error that names the accepted form, before
any model loads. Omitted, results are exactly as before. A response ends with a
`Note:` line saying the results are weighted by file age. `search` first
gathers the best 4 x (`offset + limit` + 1) files, at most 200, weights them
and then cuts the page, so a recent file just below the plain cut can appear;
as that pool grows with the page requested, a result near a page boundary can
shift by a position, as for `query`. Details and caveats (files without a
recorded mtime count as new): [Recency](CLI.md#recency).

`get` and `multi_get` also cap each document body at 256 KiB, and `multi_get`
caps the whole response at 4 MiB. A response cut short by a cap ends with a
`[truncated: …]` note (or, for `multi_get`, an `Output budget` note naming how
many documents were omitted); use `from_line`/`max_lines` on `get` to page
through the rest.

### multi_get patterns

`pattern` is a comma-separated list of entries, resolved exactly as for
`rqmd multi-get`; the full rules are in [Multi-get patterns](CLI.md#multi-get-patterns).
In short, each entry's kind comes from its form, no kind outranks another, and
the response holds the union of all matches, each document once, ordered by
`collection/path` rather than by entry order:

| Kind | Example entry | Returns |
|------|---------------|---------|
| docid | `#a1b2c3` | the document whose content hash starts with `a1b2c3` (not narrowed by `collections`) |
| glob | `notes/2025-05*.md` | every document whose `collection/path` matches |
| path | `README.md` | every document whose path or `collection/path` is `README.md` or ends with `/README.md` |

Long lists are accepted: a repeated entry is looked up once, and more than 5,000
distinct path entries in one call is an error. The 200-document and 4 MiB caps apply after
resolution, so which documents survive a cap follows that order. When a cap
drops documents, its note says how many were dropped.

Entries that need a second look add one `Note:` line each at the end of the
response, after any cap note, without changing the documents returned:

```
Note: no document matched "missing.md".
Note: "README.md" matched 3 documents; all are returned.
```

An entry of any kind is noted when it matches nothing, and a path entry also
when it matches more than one document. At most 20 entries are named; the rest
are counted in a final `Note:` line.

## Refreshing before a search

The index is only as current as the last `rqmd update` or `rqmd watch` pass, so a file edited since
then is not found. Pass `refresh: true` to `search` or `query` and the server first runs the same
incremental full-text pass as `rqmd update` for the collections the call names (all of them when it
names none), then searches. A name that matches no collection is skipped. Omitted or `false`, the
call does what it always did, and its reply is unchanged.

The pass is `rqmd update`'s own: the collection's mask, ignore patterns, hidden-file and
`.gitignore` rules, size limit and symlink containment decide which files count, new files are
added, edited files replaced and deleted files removed. It differs in what it leaves out:

- **Keyword search only.** It never loads a model and never embeds. A new or edited file is found
  by `search`, and by the keyword part of `query`, at once, while vector results do not include it
  and `Pending embed` in `status` rises until `rqmd embed` runs. Vectors that a deleted file leaves
  behind are reached by no query and are swept by the next `rqmd update` or `rqmd watch` pass.
- **Hooks never run.** A collection's `update_command` is not run by a refresh, whatever the
  index's origin: a search tool is not what the hook was set up for.
- **Bounded.** The pass stops between files once `RQMD_MCP_TOOL_TIMEOUT_SECS` (default 120) has
  passed since it began, keeps what it indexed and prunes nothing. A `query` then has its full
  timeout for itself. The directory walk of one collection is not interrupted.
- **The lock is held for the pass only.** If another process holds the index lock, or the full-text
  index still needs its one-time rebuild (which only `rqmd update` performs, so a refresh does not
  take the lock then), the pass does not run and the search does not fail: it returns the results
  the index holds, ending with one `Note:` line that says the refresh was skipped and why. A pass
  that stopped at the timeout or failed, or a refresh that is not enabled (see below), ends with a note too.
  The note names no path and does not repeat an error; the server's log has it.
- **Nothing is written to stdout** except protocol messages, so it is safe over stdio.

- **Not for an implicit project-local index.** A `.rqmd/` index picked up from the working directory can
  belong to a repo someone else authored, and a refresh walks every path its collections name. When
  the server resolved its index that way, `refresh: true` is answered with the index's results and one
  `Note:` line, and nothing is indexed. Start the server with `--allow-refresh` to opt in. An explicit
  `--index-dir` or the global index is refreshed by default, and `--no-refresh` turns the option off for
  any index. A daemon keeps the decision of the command that started it.

A refresh rewrites the index, never the collection files. Because `rqmd mcp` has no authentication
(see [Binding beyond localhost](#binding-beyond-localhost)), a client that can reach a non-loopback
server can also ask it for refresh passes, which cost a directory walk and index writes; they are
serialized by the index lock and bounded by the tool timeout.

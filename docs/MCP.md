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
can treat them as read-only (for example, run them in plan mode without a prompt). `openWorldHint`
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

On SIGTERM or Ctrl-C the server ends every open event stream before it exits, so a
connected client that holds a `GET /mcp` stream open no longer delays `rqmd mcp stop`.
Tool calls answer over the same kind of stream, so a call that has not returned yet is
cut off as well and the client gets no result for it.

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
other tools are single index lookups and have no such bound.

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
| | `rerank` | `boolean`, optional | Default `true` |
| | `expand` | `boolean`, optional | Default `true` |
| | `strict_timeout` | `boolean`, optional | Default `false`: a tool timeout that passes before reranking returns the results in fusion order with a note. `true`: it returns a `timed out` error instead |
| `search` | `query` | `string` (required) | BM25-only search text |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `limit` | `number`, optional | Default 10 |
| | `offset` | `number`, optional | Results to skip, for the next page. Default 0, capped at 1000 |
| | `per_file` | `number`, optional | Sections of one file to return, best first. Default 1, capped at 5 (see below) |
| `get` | `file` | `string` (required) | Path or `#docid` |
| | `from_line` | `number`, optional | Start line for partial retrieval |
| | `max_lines` | `number`, optional | Cap on lines returned (default 2000) |
| `multi_get` | `pattern` | `string` (required) | Glob pattern |
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

`recency` makes `query` multiply each result's score by a factor of its file's
modification time that falls from 1 toward 0.5, with the part above 0.5 halving
every half-life. Files are only down-weighted, never removed. The value is a whole
number of days, weeks or hours, above zero and at most `3650d`; anything else
returns an error that names the accepted form, before any model loads. Omitted,
results are exactly as before. A response ends with a `Note:` line saying the
results are weighted by file age. Details and caveats (files without a recorded
mtime count as new): [Recency](CLI.md#recency).

`get` and `multi_get` also cap each document body at 256 KiB, and `multi_get`
caps the whole response at 4 MiB. A response cut short by a cap ends with a
`[truncated: …]` note (or, for `multi_get`, an `Output budget` note naming how
many documents were omitted); use `from_line`/`max_lines` on `get` to page
through the rest.

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

## Binding beyond localhost

`--host` (default `127.0.0.1`, env `RQMD_MCP_HOST`) controls the bind
address for `--http`/`--daemon` mode; `--port` (default `8181`, env
`RQMD_MCP_PORT`) controls the port. `127.0.0.1`, `localhost`, and `::1` count
as loopback; anything else is non-loopback.

`--host` takes an IPv4 address, a hostname, or an IPv6 literal. IPv6 may be
given bare (`--host ::1`) or bracketed (`--host '[::1]'`); rqmd binds the
address and accepts the bracketed `Host: [::1]:<port>` form in requests. Use
`--host ::1` for IPv6 loopback — it is not served by `127.0.0.1`.

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
passed at the next stage boundary (before expansion, each expanded sub-query, or rerank). A
model call already running is not interrupted, so a call can overrun by one stage. The other
tools are single index lookups and have no such bound.

## MCP tool parameters

Exact input fields per tool, as accepted by the JSON-RPC tool call
(`collections` is plural, matching the CLI's repeatable `-c`/`--collection`):

| Tool | Field | Type | Notes |
|------|-------|------|-------|
| `query` | `query` | `string` (required) | The search text |
| | `intent` | `string`, optional | Background context steering expansion/reranking |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `limit` | `number`, optional | Default 10 |
| | `rerank` | `boolean`, optional | Default `true` |
| | `expand` | `boolean`, optional | Default `true` |
| `search` | `query` | `string` (required) | BM25-only search text |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `limit` | `number`, optional | Default 10 |
| `get` | `file` | `string` (required) | Path or `#docid` |
| | `from_line` | `number`, optional | Start line for partial retrieval |
| | `max_lines` | `number`, optional | Cap on lines returned (default 2000) |
| `multi_get` | `pattern` | `string` (required) | Glob pattern |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `max_lines` | `number`, optional | Cap on lines returned per document (default 2000) |
| `status` | *(none)* | | Takes no input. Reports document/vector counts and per-collection document counts; it does not disclose filesystem paths |

`get` and `multi_get` also cap each document body at 256 KiB, and `multi_get`
caps the whole response at 4 MiB. A response cut short by a cap ends with a
`[truncated: …]` note (or, for `multi_get`, an `Output budget` note naming how
many documents were omitted); use `from_line`/`max_lines` on `get` to page
through the rest.

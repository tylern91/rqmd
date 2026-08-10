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
| | `max_lines` | `number`, optional | Cap on lines returned |
| `multi_get` | `pattern` | `string` (required) | Glob pattern |
| | `collections` | `string[]`, optional | Scope to these collections |
| | `max_lines` | `number`, optional | Cap on lines returned per document |
| `status` | *(none)* | | Takes no input |

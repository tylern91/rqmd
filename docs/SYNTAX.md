# rqmd Query Syntax

rqmd queries are structured documents with typed sub-queries. Each line specifies a search type and query text.

## Grammar

```ebnf
query          = expand_query | query_document ;
expand_query   = text | explicit_expand ;
explicit_expand= "expand:" text ;
query_document = [ intent_line ] { typed_line } ;
intent_line    = "intent:" text newline ;
typed_line     = type ":" text newline ;
type           = "lex" | "vec" | "hyde" ;
text           = quoted_phrase | plain_text ;
quoted_phrase  = '"' { character } '"' ;
plain_text     = { character } ;
newline        = "\n" ;
```

## Query Types

| Type | Method | Description |
|------|--------|-------------|
| `lex` | BM25 | Keyword search with exact matching |
| `vec` | Vector | Semantic similarity search |
| `hyde` | Vector | Hypothetical document embedding |

## Default Behavior

A rqmd query is either a single expand query or a multi-line query document. Any single-line query with no prefix is treated as an expand query and passed to the expansion model, which emits lex, vec, and hyde variants automatically.

```
# These are equivalent and cannot be combined with typed lines:
how does authentication work
expand: how does authentication work
```

## Lex Query Syntax

Lex queries support special syntax for precise keyword matching:

```ebnf
lex_query   = { lex_term } ;
lex_term    = negation | phrase | word ;
negation    = "-" ( phrase | word ) ;
phrase      = '"' { character } '"' ;
word        = { letter | digit } ;
```

Tokenization splits on any non-alphanumeric character. An apostrophe or a
period is not part of a `word` — each acts as a token boundary, so `don't`
indexes as two tokens (`don`, `t`) and `rqmd.core` indexes as two tokens
(`rqmd`, `core`).

| Syntax | Meaning | Example |
|--------|---------|---------|
| `word` | Exact token match (no prefix/stemming/ngram matching) | `perf` matches only the token `perf` — it does **not** match "performance" |
| `"phrase"` | Exact phrase | `"rate limiter"` |
| `-word` | Exclude term | `-sports` |
| `-"phrase"` | Exclude phrase | `-"test data"` |

Multiple `lex_term`s are OR-combined by default: `auth session` matches any
document containing "auth" **or** "session", not necessarily both.
`-word`/`-"phrase"` terms are still subtracted regardless — negation is not
affected by the OR default.

### Examples

```
lex: CAP theorem consistency
lex: "machine learning" -"deep learning"
lex: auth -oauth -saml
```

## Vec Query Syntax

Vec queries are natural language questions. No special syntax — just write what you're looking for.

```
vec: how does the rate limiter handle burst traffic
vec: what is the tradeoff between consistency and availability
```

## Hyde Query Syntax

Hyde queries are hypothetical answer passages (50-100 words). Write what you expect the answer to look like.

```
hyde: The rate limiter uses a sliding window algorithm with a 60-second window. When a client exceeds 100 requests per minute, subsequent requests return 429 Too Many Requests.
```

## Multi-Line Queries

Combine multiple query types for best results. First query gets 2x weight in fusion.

```
lex: rate limiter algorithm
vec: how does rate limiting work in the API
hyde: The API implements rate limiting using a token bucket algorithm...
```

## Expand Queries

An expand query stands alone; it's not mixed with typed lines. You can either rely on the default untyped form or add the explicit `expand:` prefix:

```
expand: error handling best practices
# equivalent
error handling best practices
```

Both forms call the local query expansion model, which generates lex, vec, and hyde variations automatically.

## Intent

An optional `intent:` line provides background context to disambiguate ambiguous queries. It steers query expansion, reranking, and snippet extraction but does not search on its own.

- At most one `intent:` line per query document
- `intent:` cannot appear alone — at least one `lex:`, `vec:`, or `hyde:` line is required
- Intent is also available via the `--intent` CLI flag or MCP `intent` parameter

```
intent: web page load times and Core Web Vitals
lex: performance
vec: how to improve performance
```

Without intent, "performance" is ambiguous (web-perf? team health? fitness?). With intent, the search pipeline preferentially selects and ranks web-performance content.

## Constraints

- Top-level query must be either a standalone expand query or a multi-line document
- Query documents allow only `lex`, `vec`, `hyde`, and `intent` typed lines (no `expand:` inside)
- `lex` syntax (`-term`, `"phrase"`) only works in lex queries
- At most one `intent:` line per query document; cannot appear alone
- Empty lines are ignored
- Leading/trailing whitespace is trimmed

## Scoping

Restrict queries to specific collections with `-c` (CLI) or `collections` (MCP/SDK):

```bash
# CLI — by collection name (see `rqmd collection list`)
rqmd query -c docs "how does auth work"
rqmd query -c docs -c notes $'lex: auth\nvec: authentication flow'
```

For MCP, pass a plural `collections` array (OR match):

```json
{ "query": "lex: auth", "collections": ["docs", "notes"] }
```

`-c`/`collections` matches by collection name and works from any directory.
Multiple values are OR-combined. Without scoping, all default-included collections
are searched; collections marked excluded (`rqmd collection exclude <name>`) are
skipped unless explicitly named. In MCP the parameter is the plural `collections`
array — a singular `collection` is silently ignored.

## MCP API

The `query` tool takes the same `query` string as the CLI — plain text, or a
multi-line typed document with `lex:`/`vec:`/`hyde:`/`intent:` lines. There is no
`searches` array and no separate REST query endpoint; `query` is served over the
MCP protocol itself (stdio, or `/mcp` when running with `--http`/`--daemon` — see
`GET /health` on the same port for daemon liveness, not for search).

```json
{
  "query": "lex: CAP theorem\nvec: consistency vs availability",
  "collections": ["docs"],
  "limit": 10
}
```

With intent:

```json
{
  "query": "lex: performance",
  "intent": "web page load times and Core Web Vitals"
}
```

Other `query` fields: `rerank` (default `true`, set `false` to skip LLM
reranking) and `expand` (default `true`, set `false` to skip query-expansion/HyDE).
The `search` tool takes the same `query`/`collections`/`limit` shape for
BM25-only lookups.

## CLI

```bash
# Single line (implicit expand)
rqmd query "how does auth work"

# Multi-line with types
rqmd query $'lex: auth token\nvec: how does authentication work'

# Structured
rqmd query $'lex: keywords\nvec: question\nhyde: hypothetical answer...'

# With intent (inline)
rqmd query $'intent: web performance and latency\nlex: performance\nvec: how to improve performance'

# With intent (flag)
rqmd query --intent "web performance and latency" "performance"

# Skip query expansion — direct hybrid retrieval, no LLM round-trip.
# CLI counterpart to the MCP `expand: false` field documented above.
rqmd query --no-expand "how does auth work"
```

## Output Format

`--format` accepts `cli` (default), `json`, `csv`, `md` (alias `markdown`),
`xml`, or `files`; see the README's CLI reference for what each renders.
Format choice does not change how query syntax is parsed — the rules above
apply the same regardless of `--format`.

## `rqmd similar`

`rqmd similar <path>` takes a document reference — a file path or `#docid` —
not a query string. None of the query-syntax rules on this page (lex/vec/hyde
typed lines, expand, intent) apply to it.

# Security Policy

## Supported versions

rqmd releases from `main` only; there are no maintained release branches.
Security fixes land in the latest release.

## Reporting a vulnerability

Please report suspected vulnerabilities privately, using
[GitHub's private vulnerability reporting](https://github.com/tylern91/rqmd/security/advisories/new)
for this repository, rather than opening a public issue. This lets a fix land
before the details are public.

Include what you'd include in a bug report: repro steps, affected version
(`rqmd --version`), and impact. Response time isn't guaranteed — this is a
single-maintainer project — but reports will be triaged.

## Automated scanning

Every PR runs [Trivy](https://github.com/aquasecurity/trivy) against the
dependency tree (`.github/workflows/security.yml`). CRITICAL-severity
vulnerabilities with a known fix block the merge; HIGH-severity findings are
recorded to the repository's Security tab for tracking but don't block.

## Known design boundary: the MCP HTTP listener has no authentication

This is documented behavior, not an unreported vulnerability — please don't
file it as one. `rqmd mcp --http` (and `--daemon`) expose query, search, and
file-read access to the indexed corpus with **no authentication of any
kind**. Anyone who can reach the bound host:port can read every indexed
document.

The listener binds to `127.0.0.1` by default. `rqmd mcp --http`/`--daemon`
**refuses to start** if `--host` (or `RQMD_MCP_HOST`) resolves to anything
other than `127.0.0.1`, `localhost`, or `::1`, unless you also pass
`--allow-non-loopback` (or set `RQMD_MCP_ALLOW_NON_LOOPBACK=1`) — a deliberate
confirmation gate, not a warning you can miss. Passing it should be treated
as exposing the corpus to that network, not as a convenience flag — see
[docs/MCP.md](docs/MCP.md#binding-beyond-localhost) for the exact refusal
text and the full list of exposed tools, and
[docs/CONFIGURATION.md](docs/CONFIGURATION.md) for the env var.

If you need authenticated or multi-tenant access to an rqmd index, that isn't
implemented today; a proposal is welcome as an issue.

## Update hook trust

A collection can carry an `update_command` (set via `rqmd collection
update-cmd`) that `rqmd update` runs through the shell before re-indexing that
collection — e.g. `git pull` to refresh a synced directory. That command is
read from the resolved index's `index.sqlite`, so which index gets resolved
matters:

- `--index-dir`/`RQMD_INDEX_DIR` (explicit) and the global `~/.cache/rqmd/`
  index are ones you pointed rqmd at yourself — their hooks run by default.
- A project-local `.rqmd/` picked up implicitly from the current directory
  can belong to a repo someone else authored. Its `update_command` is
  **skipped by default** (with a warning) unless you pass `rqmd update
  --run-hooks` — otherwise, running `rqmd update` right after cloning an
  unfamiliar repo that ships a `.rqmd/` would execute a command chosen by
  that repo's author, not you.

If you do trust the repo, `--run-hooks` opts back in for that invocation.

The same rule gates the MCP `refresh: true` option, which walks every path an
index's collections name: it is answered with a note for an implicit
project-local `.rqmd/` unless `rqmd mcp --allow-refresh` is passed, and a refresh
never runs a hook for any index.

## Collections outside the project

A project-local `.rqmd/index.sqlite` also stores each collection's `path`, and
`rqmd update`, `rqmd watch` and the MCP `refresh: true` option copy the matching
files under that path into the index, wherever it points. In a repo someone else
authored, that can name a directory such as your home directory, and a later
`search`, `get` or MCP call returns what was copied.

On a project-local index, these three skip every collection whose directory
resolves outside the project directory (the current directory that holds
`.rqmd/`) unless you pass `--allow-external-paths`. The skip comes before the
update hook and the directory walk, so nothing is read from the directory, and
nothing is pruned: documents the collection already holds stay in the index and
stay searchable. Each skipped collection is named once on stderr:

```
  WARN: skipping collection '<name>' (<path>): outside the project directory; pass --allow-external-paths to index it
```

The run goes on with the other collections and ends as it does for a missing
directory, with exit status 0. A relative stored path is read against the project
directory, a symlinked root is judged by its target, and a path that does not
exist keeps only its `directory not found` warning. Control characters in the
printed name and path are replaced. `rqmd watch` and the MCP server skip the
collection on every pass and name it once per process.

`--allow-external-paths` is accepted by `rqmd update`, `rqmd watch` and
`rqmd mcp`. It applies to that invocation only and is not stored in the index.
`rqmd mcp --allow-refresh` does not imply it, so accepting refreshes and
accepting reads outside the project stay separate decisions. An
`rqmd mcp --daemon` keeps the decision of the command that started it. An
explicit `--index-dir` and the global index are chosen by you and are not
checked, so `rqmd --index-dir .rqmd update` remains a way to index an outside
collection.

`rqmd collection list` and `rqmd collection show` mark such a collection
`(outside project; skipped)`, `rqmd doctor` marks it and warns, and `rqmd status`
prints a count. Design and threat model:
[docs/research/2026-10-27-external-collection-paths.md](docs/research/2026-10-27-external-collection-paths.md).

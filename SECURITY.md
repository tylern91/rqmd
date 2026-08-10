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

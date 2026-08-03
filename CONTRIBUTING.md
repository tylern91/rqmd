# Contributing to rqmd

Thanks for considering a contribution. This document covers everything a new
contributor needs to land a correct PR without having to ask.

## ⚠️ Before your first commit: GPG signing is mandatory

`main` is protected by a repository ruleset that requires every commit to be
GPG-signed (`required_signatures`, `bypass_actors: []`) — an unsigned commit
is rejected at push, not just at merge. Set up commit signing before you push
anything:

```sh
git config commit.gpgsign true
git config user.signingkey <your-key-id>
```

The same ruleset also enforces:

- **Squash-merge only** — the target branch never sees your individual
  commits, only one squashed commit per PR.
- **Linear history** — no merge commits.
- No force-push, no branch deletion on `main`.
- 0 required approvals (the maintainer merges solo today, but every other
  gate above still applies).

## Design principles

These aren't aspirational — they're enforced by CI or by the shape of the
code. Know them before proposing a change that cuts against one:

- **Local-first.** rqmd sends no telemetry. The only network access is the
  first-run HuggingFace model download and, for the `ort-backend` feature, a
  build-time ONNX Runtime fetch — both are disableable with `HF_HUB_OFFLINE=1`.
- **Search quality is the contract.** RRF fusion (`k=60`, weight 2.0 for the
  original query vs 1.0 for expansions), BM25 field boosts (filepath 1.5 /
  title 4.0 / body 1.0), and the 3600/540 chunking window are tuned values,
  not defaults to nudge casually. Read `BENCHMARK.md` before touching the
  backend or the search DB — it says so at the top for a reason.
- **Never block on a missing model.** `search`, `get`, `ls`, and `similar`
  all run against `NoBackend`; `update` touches only the BM25 index so
  ingestion works with zero models loaded. Don't add a code path that makes
  these commands require an embedding model.
- **Single static binary.** The whole point is no Node, no Bun, no
  native-module rebuild per platform. A new dependency needs to justify
  itself against the ~60 MB budget.

## Ways to contribute

| Type | How |
|---|---|
| Report a bug | Open an issue with repro steps, `rqmd doctor` output, and OS/arch |
| Fix a bug | See the local gate below, then open a PR |
| Add a feature | Consider opening an issue first for anything touching retrieval, fusion, or the CLI surface |
| Review a PR | Check it against the design principles above, not just style |
| Improve docs | README, `docs/SYNTAX.md`, and this file all welcome fixes |

## Commit convention

[Conventional Commits](https://www.conventionalcommits.org/), using the types
and scopes actually in use in this repo:

- **Types:** `feat`, `fix`, `chore`, `ci`, `docs`, `perf`
- **Scopes:** `cli`, `mcp`, `embed`, `index`, `retrieval`, `llm`, `store`,
  `query`, `release`, `security`, `chunking`, `context`, `doctor`,
  `collection`, `format`, `packaging`
- **Multiple scopes:** comma-join them — `fix(progress,status): ...`
- Bare `docs:` / `chore:` with no scope are fine.

**Because merges are squash-only, your PR title becomes the commit message.**
Release automation regexes the title for breaking-change escalation (see
below), so title it as you'd want it to read in `CHANGELOG.md`.

## Branch naming

`<type>/<kebab-case-slug>` — `fix/`, `feat/`, `docs/`, `ci/`, and `chore/` are
all in use. This is a convention, not an enforced gate.

## Local gate before pushing

Run the exact commands CI runs, before you push:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace --lib
cargo run --bin rqmd -- eval --mode bm25
```

**CI only triggers on code changes.** `rust.yml` is path-filtered to
`crates/**`, `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `CHANGELOG.md`,
`scripts/check-version-sync.sh`, and its own file — a docs-only PR (like this
one) runs no Rust CI at all. `security.yml` (Trivy) runs on every PR
regardless; only a **CRITICAL** vulnerability with a known fix blocks the
merge, HIGH severity is recorded to the Security tab but doesn't block.

## Version + CHANGELOG convention

This is the part that isn't written down anywhere else, so read it carefully:

A PR that should ship in a release **finalizes its own release section** in
`CHANGELOG.md`:

1. Add `## [X.Y.Z] - YYYY-MM-DD` above the previous release heading.
2. Bump `version` under `[workspace.package]` in the root `Cargo.toml` to
   match `X.Y.Z` exactly. `scripts/check-version-sync.sh` fails CI if the two
   disagree.
3. Leave `## [Unreleased]` in the file — **empty**, as a permanent
   placeholder. Never delete it: the release workflow's empty-notes guard
   silently no-ops a release if `## [Unreleased]` is missing entirely.
4. Use the existing buckets — `### Added` / `Changed` / `Fixed` /
   `Documentation` — followed by a `---`. Write bullets as prose explaining
   *why* the change matters, hand-wrapped at roughly 76 columns, matching the
   rest of the file.

## Semver labels

Apply exactly one on your PR: `patch`, `minor`, or `skip-release`.
**A PR with no label produces no release** — the release workflow simply
doesn't run.

Docs-only changes (like this one) use `skip-release`, **not** `patch` —
`patch` fails the changelog gate on a PR that adds no `CHANGELOG.md` entry.

## Breaking changes

Escalate a `major`/`minor` label to a major version bump with any of:

- A PR title matching `^[a-z]+(\([^)]+\))?!:` (e.g. `feat(mcp)!: ...`)
- A `BREAKING CHANGE:` line in the PR body
- The `breaking-change` label

## No CLA, no DCO, no sign-off

Your PR is not gated on signing a Contributor License Agreement or adding a
`Signed-off-by` trailer. Submitting a PR is enough.

## Don't touch — looks usable, isn't

A few things in the repo look like working tooling but currently aren't safe
to rely on:

- **`scripts/pre-push`** — checks each crate's version against the pushed
  tag with `grep '^version = '`, but every crate manifest now uses
  `version.workspace = true` — the pattern never matches, so the hook would
  block every tag push. Don't install it as-is.
- **`scripts/crosscheck.sh`** — compares Rust rqmd against TypeScript qmd on
  a shared fixture corpus; its path constants predate the current `crates/`
  layout.
- **`.github/workflows/nix.yml`** — triggers only on a `release` branch that
  doesn't exist in this repo, so it never runs today.

If you want to fix one of these, that's a welcome PR on its own — just don't
assume it currently works.

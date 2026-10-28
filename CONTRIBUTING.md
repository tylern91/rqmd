# Contributing to rqmd

Thanks for considering a contribution. This document covers everything a new
contributor needs to land a correct PR without having to ask.

Everyone taking part in rqmd's issues, pull requests and discussions is
expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md). If you change
what rqmd prints or how it is documented, see [ACCESSIBILITY.md](ACCESSIBILITY.md).

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
| Improve docs | README, `docs/`, and this file all welcome fixes |

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

**Every PR reports `ci-complete`.** `rust.yml` has no `paths:` filter. Its
`changes` job checks whether the PR touches `crates/**`, `Cargo.toml`,
`Cargo.lock`, `.cargo/**`, `CHANGELOG.md`, the version-sync and acceptance
scripts, `scripts/ci-rust-changed.sh` or `rust.yml` itself, and skips the heavy
jobs when it does not, so a docs-only PR ends with a green `ci-complete` and no
Rust build. Pushes to `main`, the weekly run and manual dispatches always run
everything. A weekly (and manually dispatched) run also executes the full suite
on macOS, including the model-download test that the per-PR jobs skip.
`lock-soak.yml` runs only when a PR changes `Cargo.lock` or the soak script (see
"Dependency updates"). `msrv-track.yml` runs on a weekly schedule, and on a PR only to self-test
its script, when the PR changes the script or a file it edits (see "MSRV policy"). `security.yml` (Trivy) runs on every PR regardless; only a **CRITICAL** vulnerability with a known fix blocks the
merge, HIGH severity is recorded to the Security tab but doesn't block.

## Acceptance-test tier

`crates/rqmd-cli/tests/acceptance/` holds end-to-end tests that drive the
real `rqmd` binary via `assert_cmd`, one file per cluster (e.g.
`collection.rs`). This sits alongside — not instead of — the existing inline
`#[cfg(test)]` unit tests; the two tiers coexist.

- **Naming**: every test is `fn ac_<N>_<behaviour>()`, where `N` is the
  **1-based position** of the acceptance-criteria checkbox it proves, in the
  linked issue's `## Acceptance Criteria` section. Issue #66's two boxes are
  AC-1 and AC-2.
- **Lint** (`scripts/lint-acceptance-tests.sh`, run in the `ac-gate` CI job):
  Rule A rejects a test function not matching `^ac_[0-9]+_`; Rule B rejects
  an `assert_cmd` result that's never interrogated (no `.success()` /
  `.failure()` / `.stdout(…)` / `.stderr(…)` chained) — the equivalent of a
  bats `[ ]` that silently no-ops.
- **Revert-ledger obligation**: any `ac_N` test added or changed must have a
  row in `crates/rqmd-cli/tests/acceptance/revert-ledger.tsv`
  (`suite<TAB>ac<TAB>verified_sha<TAB>date`), earned by actually mutating the
  production code the test exercises, observing the test fail, reverting the
  mutation, byte-diffing the reverted file clean against a pre-mutation
  backup, and observing the test pass again. This is not optional
  documentation — CI's `ac-gate` job checks it against `origin/main` on every
  PR that touches the tier.
- **Fold / Keep / Delete**: when a future PR migrates an existing unit-test
  cluster into this tier, give every original test in that cluster one of
  three verdicts, recorded in `docs/research/<date>-atdd-migration-<cluster>-cluster.md`:
  **Fold** (an acceptance test already reaches the same branch — delete the
  unit test), **Keep** (guards a branch no acceptance test reaches — stays in
  the unit tier), or **Delete** (provides no coverage at all). Deleting a
  test without a written verdict is not allowed.

## MSRV policy

`rust-version = "1.99"` in the root `Cargo.toml` (`[workspace.package]`,
inherited by every crate) is the **newest stable Rust minor**. The Rust
project patches only its newest stable release and has no long-term-support
line, so every older release is end of life and rqmd does not support it. The
MSRV therefore follows each stable release, raised within days of it.

- The max `rust-version` any dependency declares (`cargo metadata --locked
  --offline`) is only a lower bound: the MSRV may never be below it, but it
  is not what sets the MSRV.
- Raising the MSRV is a **minor** version bump — it can break users on older
  toolchains, same as the `RQMD_` env var rename in v0.11.0. Homebrew users
  are not affected (the formula installs a prebuilt bottle); `cargo install`,
  source builds and the Nix dev shell are.
- A raise touches the same places every time: `Cargo.toml`, the MSRV leg in
  `rust.yml`, `docs/INSTALL.md` (two lines), the `flake.nix` comment, and
  `flake.lock` (`nix flake update nixpkgs`, so the dev shell's `rustc` meets
  the new floor). Use `rust-version = "1.N"`, not `"1.N.P"`, so a nixpkgs
  that ships `1.N.0` still meets it.
- **MSRV tracker.** `.github/workflows/msrv-track.yml` makes that raise
  without anyone remembering it. Every Monday (and on manual dispatch) it
  reads the newest stable minor from
  `https://static.rust-lang.org/dist/channel-rust-stable.toml` and compares it
  with `rust-version`. When the stable minor is newer and `nixpkgs-unstable`
  already has `pkgs/development/compilers/rust/1_<minor>.nix`, it makes the
  edits above, adds a CHANGELOG "Changed" line, and opens one PR on branch
  `chore/msrv-<minor>` labelled `minor` (or refreshes the open one). If
  nixpkgs is behind, the run ends green and tries again the next week, so the
  Nix dev shell never breaks. It opens the PR with the repository's GitHub App
  token (the one `release.yml` uses), because a PR opened with `GITHUB_TOKEN`
  starts no CI. The tracker never merges. A person merges after CI and clippy
  on the new stable pass, and fixes any new lint in that PR: the tracker
  refreshes the branch only while every commit on it is its own, so a pushed
  fix is never overwritten. Close the PR without merging to skip that minor.
  The logic is in `scripts/msrv-track.sh`; run its offline tests with
  `bash scripts/tests/msrv-track.test.sh`. The script fails, rather than
  guessing, when one of the places listed above no longer matches, so change
  the script's edit list in the same PR that moves any of them (that PR's
  tracker self-test fails until you do).
- There is no `rust-toolchain.toml`. CI builds on stable, and `rust.yml`'s
  `build-linux` job also runs a `["stable", "1.99"]` toolchain matrix; the
  pinned leg only runs `cargo check --workspace --all-targets` (no tests) and is
  advisory (`continue-on-error`), so a real regression shows as a failed step in
  the PR checks without blocking the merge.
- The edition is 2024, which needs Rust ≥1.85 — far below the floor, so it
  isn't itself an MSRV constraint.

## Dependency updates

A new dependency's license must be in the `accepted` list in `about.toml`;
the `Third-party licenses` CI job fails otherwise. Extend that list only after
checking what the license asks of a binary distribution. The job also fails
when a shipped crate's license text cannot be found, and each release
generates `THIRD-PARTY-LICENSES.md` with the same script
(`scripts/gen-third-party-licenses.sh`).

`.github/dependabot.yml` opens weekly update PRs for `github-actions` and
`cargo`. Both ecosystems set `cooldown: default-days: 30`, so Dependabot
skips a release until it has been public for 30 days. That matches the
manual rule for any dependency bump: move only to a stable release that is at
least 30 days old, and pin each crate with
`cargo update -p <crate> --precise <version>` instead of a blanket
`cargo update`.

- Security updates ignore the cooldown. They still need a maintainer's review
  of the lock diff, and the 30-day soak is waived only for a critical
  (CVSS >= 9.0 or RCE-class) advisory that names the smallest patched version.
- `llama-cpp-2`, `llama-cpp-sys-2`, `usearch`, `tantivy*`, `tokenizers`,
  `ort`, `ort-sys`, `tree-sitter*`, `rusqlite` and `libsqlite3-sys` are on
  Dependabot's `ignore` list because they can change stored data (index
  layout, vector format, tokenization, schema). They only move by hand, in
  their own reviewed change. `ignore` also holds back security updates, so an
  advisory against one of them is handled manually.
- **Lock soak gate.** `.github/workflows/lock-soak.yml` runs on every PR that
  changes `Cargo.lock` and calls `scripts/check-lock-soak.sh`. For each crate
  version the PR adds or changes, direct or transitive, it asks the crates.io
  API and fails unless the version was published at least 30 days ago and is
  not yanked. A lookup that cannot be completed also fails. A young
  transitive crate is the usual cause: pin it back with
  `cargo update -p <crate> --precise <version>`. To see the verdicts without
  failing, run
  `bash scripts/check-lock-soak.sh --dry-run <old Cargo.lock> <new Cargo.lock>`
  (for example `git show origin/main:Cargo.lock > /path/to/old.lock`). Only
  crates.io crates are checked: a git or path dependency is skipped.
- **Waiver.** A maintainer can add the `soak-waiver` label to a PR for a
  critical advisory (see above). The job then still prints every verdict but
  does not fail. Name the advisory in the PR body.
- Dependabot adds no labels (`labels: []`), so a dependency bump does not
  start a release on its own. Apply `patch`, `minor` or `skip-release` by hand
  as described under "Semver labels".

### llama.cpp (`llama-cpp-2`) bump gate

The embed fingerprint (`base_signature()` in `crates/rqmd-core/src/store.rs`)
names the model, chunking and prompts, but not the inference engine. When the
unchanged chunks of an edited document reuse their stored vectors, a vector
from the old engine and one from the new engine would sit under one
fingerprint. A `llama-cpp-2` bump (which moves the bundled llama.cpp and
`llama-cpp-sys-2` with it) therefore merges only after every step below passes.
The first bump PR links
[#151](https://github.com/tylern91/rqmd/issues/151) and records the measured
result (the printed cosine table and which builds were run). A bump changes
nothing in the fingerprint; this gate measures whether it needed to.

1. **Embedding sample.** The fixed sample is
   `crates/rqmd-llm/tests/fixtures/engine_bump_sample.json`: a short English
   text, a long text of about 2000 tokens (just under the 2044-token
   truncation limit), a longer one that is truncated, Chinese and Japanese text, a Rust snippet and a Python
   snippet. Every text is embedded as a passage and as a query (14 vectors),
   with the same model file on both sides. Change the sample only in its own
   PR, because a changed sample cannot be compared with an older reference.
2. **Measure the old and the new engine.** Run the ignored test with the
   embedding GGUF in the Hugging Face cache (`HF_HUB_OFFLINE=1` forbids a
   download; the backend resolves all three default models, as the CLI does).
   Use a file outside the repository for the reference. Run both
   steps on the same machine, the same backend (GPU or `RQMD_FORCE_CPU=1`) and
   the same model file: a GPU and the CPU already differ in the last digits on
   one engine (a Metal-versus-CPU self-check measured a lowest cosine of
   0.9994), so mixing backends across the two steps eats most of the margin and
   can fail a harmless bump.

   ```bash
   export RQMD_ENGINE_GATE_REFERENCE=/path/to/engine-gate-reference.json
   # On main, before the bump (old engine). Refuses to overwrite an existing file.
   RQMD_ENGINE_GATE_LABEL=llama-cpp-2-<old> \
     cargo test -p rqmd-llm --test engine_bump_gate -- --ignored --nocapture engine_gate_capture
   # On the bump branch (new engine).
   RQMD_ENGINE_GATE_LABEL=llama-cpp-2-<new> \
     cargo test -p rqmd-llm --test engine_bump_gate -- --ignored --nocapture engine_gate_compare
   ```

   `engine_gate_compare` prints the cosine of every old/new pair and the
   lowest one, and exits non-zero when any pair is below the threshold, when an
   id is missing or extra, or when a vector cannot be compared. The cosine,
   threshold and pairing logic has ordinary tests that need no model and run in
   CI; the two model steps never run in CI.
3. **Pass rule.** The cosine similarity of **every** old/new pair is at least
   **0.999**. One pair below it fails the gate. The mean is not enough.
4. **If it fails.** `rqmd embed --rebuild` is the only correct upgrade path.
   Reusing stored chunk vectors across engines is not acceptable, because the
   fingerprint cannot tell them apart. The bump PR must say so in its
   `CHANGELOG.md` entry and in the PR body (every user re-embeds once), and
   must not ship until the fingerprint change or another mechanism keeps
   chunk reuse from mixing engines. That is a separate change under the
   `rqmd-embed-fingerprint-changes` rules.
5. **Build checklist.** Build the new version with each GPU feature of
   `crates/rqmd-llm/Cargo.toml`. A build only proves the C++ compiles and
   links; a run is what the gate measures. The PR records, per row, whether it
   was run or only compiled, and names the machine.

   | Build | Command | How it is verified |
   | --- | --- | --- |
   | Metal (default on macOS) | `cargo build -p rqmd-llm` | Run: the steps above on an Apple Silicon Mac, once with the default GPU path and once with `RQMD_FORCE_CPU=1` for the CPU path. |
   | CUDA | `cargo build -p rqmd-llm --features cuda` | Compiled on a host with the CUDA toolkit. Run only on an NVIDIA machine; otherwise record "compiled only". |
   | Vulkan | `cargo build -p rqmd-llm --features vulkan` | Compiled on a host with the Vulkan SDK. Run only with a Vulkan GPU; otherwise record "compiled only". |

   CI builds the default feature set and `ort-backend`, so it does not cover
   CUDA or Vulkan. A backend that was only compiled is stated as such in the PR, not
   implied to be tested. Repeat both steps for each backend you can run (for example Metal,
   then `RQMD_FORCE_CPU=1`), each time with its own reference file.
6. **Soak re-check.** `lock-soak.yml` covers the new `llama-cpp-2` and every
   transitive crate the PR adds or changes (`llama-cpp-sys-2`, `cc`, `cmake`,
   `bindgen` and so on). Run
   `bash scripts/check-lock-soak.sh --dry-run <old Cargo.lock> <new Cargo.lock>`
   before opening the PR and pin a young crate back with
   `cargo update -p <crate> --precise <version>`.
7. **Release notes.** Read the `llama-cpp-2` and llama.cpp release notes for
   every version between the old and the new one, and list in the PR what
   touches rqmd's use: embedding output (pooling, normalization, the encoder
   path), tokenization (vocabulary and BOS handling) and the sampler API used
   by `generate`. "Nothing relevant found" is a finding to write down too.

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

`release.yml` also re-checks this at tag time via
`scripts/check-version-sync.sh --expect <next-tag>`, comparing `Cargo.toml`
against the version the release job is *about to cut* — not just the
CHANGELOG's top heading. This catches the case where an earlier release PR's
CI run never executed (workflow runs on bot-authored PRs, e.g. Copilot or
Dependabot, require a maintainer to click "Approve and run") and a later PR's
release then computes a stale tag against a newer commit. **Approve
bot-authored PRs' workflow runs promptly**, or re-run the missed release via
`workflow_dispatch` before merging anything else — otherwise the guard blocks
the next release until the mismatch is resolved.

### Releases wait for green CI

The `main` ruleset has no required status checks, so `release.yml` checks CI
itself. Just before it creates the tag it runs
`bash scripts/require-green-ci.sh <sha> <last tag>` on the commit it is about
to tag, for both the merge-triggered and the `workflow_dispatch` release:

- `Security Scan` must have concluded `success` on that exact commit.
- `Rust CI` must have too, unless every commit since the last tag leaves out
  the paths that `scripts/ci-rust-changed.sh` lists (a docs-only range). When
  the range cannot be computed, `Rust CI` is required.
- The script polls `gh run list --commit <sha>` every 20 seconds for up to 15
  minutes, because a merge starts the release and the push-to-`main` CI at the
  same moment. It fails at once on `failure`, `cancelled` or any other
  non-success conclusion, and at the deadline when a run is still going or
  never appeared. A re-run of a cancelled run is waited for; a newer run of
  the same workflow replaces an older one.
- A failed release is retried by fixing CI (or re-running the failed workflow
  on that commit) and then dispatching `release.yml` again.
- A dry run (`dry_run` input, or `<!-- release-dry-run -->` in the PR body)
  checks once without waiting and only warns, so it still previews the notes.

The workflow token needs `actions: read` for this (`release.yml` declares it).
The script's tests run offline with a fake `gh`:
`bash scripts/tests/require-green-ci.test.sh`.

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

## Keeping `target/` bounded

Cargo never garbage-collects `target/` — stale fingerprints accumulate
permanently across rebuilds. `scripts/clean-target.sh` prunes artifacts
(most aggressively `target/debug/incremental/`) untouched for more than 14
days (`--days N` to override, `--dry-run` to preview). Run
`scripts/install-git-hooks.sh` once per clone to chain it into
`.git/hooks/post-merge` — it runs `git lfs post-merge` first, preserving its
exit status, then the prune advisory-only (a cleanup failure never fails the
merge). The installer refuses to overwrite a post-merge hook it doesn't
recognize, and re-running it is a no-op once installed.

The script also runs a **hash-group sweep** on every invocation: a single
crate can accumulate many `target/debug/.fingerprint/<crate>-<hash>` variants
within a single day (one per feature/profile combination it's built with),
well inside the 14-day age window above. Once a newer hash appears for a
crate, the sweep removes older sibling hashes (and their matching
`target/debug/deps/`/`target/debug/build/` outputs) once they've sat
untouched for `--grace-hours` (default 24) — it never removes a crate's only
current fingerprint, so a rebuild is never forced by pruning alone. Disable
it with `--no-hash-sweep` if you need to compare hash-suffixed artifacts
across builds.

`[profile.dev]` and `[profile.dev.package."*"]` in the root `Cargo.toml` set
`debug = false` — debug builds don't need debug symbols for day-to-day
`cargo build`/`test`/`clippy` runs, and skipping them cuts per-crate
`deps/`/`.fingerprint/` size substantially. If you need symbols for a
debugger session, override it for that build only:
`CARGO_PROFILE_DEV_DEBUG=true cargo build`.

## Optional: RTK (condensed AI-agent command output)

If you drive this repo through an AI coding agent (Claude Code, Codex CLI,
etc.), you may notice `AGENTS.md`, `CLAUDE.md`, `RTK.md`, `.rtk/filters.toml`,
and `.codex/hooks.json` in the repo root. These belong to
[RTK](https://github.com/rtk-ai/rtk), a CLI proxy some contributors use to
condense noisy command output before it reaches the agent's context. None of
it does anything without the `rtk` binary installed and its agent hook
configured — without that, `AGENTS.md`/`CLAUDE.md` just point at an inert
`RTK.md`, and `.rtk/filters.toml` sits unused. Nothing here affects `cargo
build`/`test`/`clippy` or CI.

`.rtk/filters.toml` ships one project filter (`rqmd-status`, compacting
`rqmd status`'s per-collection detail). RTK does not apply a project filter
until you explicitly trust it — run `rtk trust` once (and again after editing
the filter) to review and enable it.

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

If you want to fix one of these, that's a welcome PR on its own — just don't
assume it currently works.

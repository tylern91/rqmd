## Summary

<!-- What changed, and why. Link an issue if there is one. -->

## Test plan

<!-- Check off what you actually ran. See CONTRIBUTING.md for the full local gate. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo build --workspace`
- [ ] `cargo test --workspace --lib`
- [ ] `cargo run --bin rqmd -- eval --mode bm25` (search-quality gate — must not regress)

<!--
## Migration

Optional — only if this PR changes on-disk format, config keys, env vars, or
CLI flags in a way users need to act on. If present, this section is copied
verbatim into the GitHub release notes by scripts/build-release-notes.sh.
-->

---

**Before merging, apply exactly one label:** `patch`, `minor`, or
`skip-release` (docs-only changes use `skip-release`, not `patch` — see
CONTRIBUTING.md). If this is a breaking change, also add `breaking-change` or
use a `!` in the PR title (`feat(scope)!: ...`).

**Because merges are squash-only, this PR's title becomes the commit
message** — title it accordingly.

To preview the release notes this PR would produce without actually tagging
a release, add this exact line anywhere in this PR body: `<!-- release-dry-run -->`

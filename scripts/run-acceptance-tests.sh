#!/usr/bin/env bash
# run-acceptance-tests.sh — Run the acceptance tier and record a per-test
# pass/fail ledger the CI ac-gate can check for orphans and skips.
#
# Why: `cargo test`'s own exit code only says "something failed somewhere".
# The gate needs to know *which* ac_N tests ran and whether each passed, so
# it can catch a test that silently never ran at all — the same failure
# class dotfiles' tests/bats-test-reporter.sh exists to catch.
#
# Note: libtest's `--format terse` prints one character per test (`.`, `F`,
# `i`), not test names — useless for building a named ledger. This uses the
# default pretty format instead, which prints one
# `test <suite>::<name> ... ok|FAILED|ignored` line per test.
#
# Usage: run-acceptance-tests.sh [path-to-repo-root]
# Output: <root>/acceptance.json (gitignored — a build artifact, not source)
set -Eeuo pipefail

root="${1:-.}"
out="${root}/acceptance.json"
run_log="$(mktemp)"
trap 'rm -f "$run_log"' EXIT

# `cargo test` exits non-zero on any failing test — capture the log first,
# decide pass/fail from the parsed ledger, not from this exit code alone.
(cd "$root" && cargo test -p rqmd-cli --test acceptance --locked) > "$run_log" 2>&1 || true

cat "$run_log"

entries="$(grep -E '^test [a-zA-Z0-9_:]+ \.\.\. (ok|FAILED|ignored)$' "$run_log" || true)"

if [[ -z "$entries" ]]; then
  if find "${root}/crates/rqmd-cli/tests/acceptance" -maxdepth 1 -name '*.rs' \
       -not -name 'helpers.rs' -print0 2>/dev/null | xargs -0 grep -lq '^fn ac_[0-9]' 2>/dev/null; then
    printf 'run-acceptance-tests: ac_* tests exist on disk but no test result lines were parsed — the suite did not run\n' >&2
    exit 1
  fi
fi

{
  printf '{"acceptance":{'
  first=1
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    name="$(printf '%s\n' "$line" | sed -E 's/^test ([a-zA-Z0-9_:]+) \.\.\. .*/\1/')"
    state_raw="$(printf '%s\n' "$line" | sed -E 's/^test [a-zA-Z0-9_:]+ \.\.\. (ok|FAILED|ignored)$/\1/')"
    case "$state_raw" in
      ok) state="passed" ;;
      FAILED) state="failed" ;;
      ignored) state="ignored" ;;
      *) state="unknown" ;;
    esac
    # Key format: <suite>::AC-<N>, e.g. collection::AC-1, matching the
    # composite key dotfiles uses to avoid a flat-id collision.
    ac_num="$(printf '%s\n' "$name" | sed -E 's/.*::ac_([0-9]+)_.*/\1/')"
    suite="$(printf '%s\n' "$name" | sed -E 's/::ac_[0-9]+_.*//')"
    key="${suite}::AC-${ac_num}"
    [[ $first -eq 0 ]] && printf ','
    printf '"%s":{"state":"%s","fn":"%s"}' "$key" "$state" "$name"
    first=0
  done <<< "$entries"
  printf '}}'
} > "$out"

printf 'run-acceptance-tests: wrote %s\n' "$out"

#!/usr/bin/env bash
# lint-acceptance-tests.sh — Enforce the acceptance-tier conventions over
# crates/rqmd-cli/tests/acceptance/**.rs.
#
# Why: the acceptance tier only proves anything if (a) every test is
# traceable to the acceptance criterion it satisfies, and (b) every test
# actually interrogates the command it ran rather than just running it.
# Neither is enforced by the Rust compiler, so this script checks both.
#
# Rule A — naming: every `#[test]`/`#[tokio::test]` fn must match
# `^ac_[0-9]+_`, so `N` is greppable back to a GitHub issue's Acceptance
# Criteria checkbox. helpers.rs is exempt (it declares no tests).
#
# Rule B — vacuous assertions: an `assert_cmd` `.assert()` call that is
# never interrogated (no `.success()`/`.failure()`/`.stdout(...)`/
# `.stderr(...)` chained, or discarded via `let _ = ...assert...`) is a
# test that runs the binary and checks nothing.
#
# Usage: lint-acceptance-tests.sh [path-to-repo-root]
set -Eeuo pipefail

root="${1:-.}"
dir="${root}/crates/rqmd-cli/tests/acceptance"

if [[ ! -d "$dir" ]]; then
  printf 'lint-acceptance-tests: missing directory %s\n' "$dir" >&2
  exit 1
fi

fail=0

while IFS= read -r -d '' file; do
  base="$(basename "$file")"
  [[ "$base" == "helpers.rs" ]] && continue

  # Rule A: the fn on the line after #[test] / #[tokio::test] must match
  # ^ac_[0-9]+_. attr_pending tracks "the previous non-blank line was a
  # test attribute" across intervening lines like doc comments.
  attr_pending=0
  line_no=0
  while IFS= read -r line; do
    line_no=$((line_no + 1))
    trimmed="${line#"${line%%[![:space:]]*}"}"
    if [[ "$trimmed" =~ ^#\[(tokio::)?test\] ]]; then
      attr_pending=1
      continue
    fi
    [[ -z "$trimmed" ]] && continue
    if [[ $attr_pending -eq 1 ]]; then
      if [[ "$trimmed" =~ ^(pub[[:space:]]+)?(async[[:space:]]+)?fn[[:space:]]+([a-zA-Z0-9_]+) ]]; then
        fn_name="${BASH_REMATCH[3]}"
        if [[ ! "$fn_name" =~ ^ac_[0-9]+_ ]]; then
          printf '%s:%d:NAME: fn %s does not match ^ac_[0-9]+_\n' "$file" "$line_no" "$fn_name" >&2
          fail=1
        fi
      fi
      attr_pending=0
    fi
  done < "$file"

  # Rule B: a `.assert()` call with no chained interrogation, or one
  # whose result is discarded outright. A chain may continue across
  # several lines (`.assert()\n.success();`), so scan forward from the
  # `.assert()` line up to the statement's terminating `;`.
  total_lines="$(wc -l < "$file" | tr -d ' ')"
  while IFS=: read -r fline lcontent; do
    if [[ "$lcontent" =~ let[[:space:]]+_[[:space:]]*=.*assert ]]; then
      printf '%s:%s:VACUOUS: assert result discarded via let _ =\n' "$file" "$fline" >&2
      fail=1
      continue
    fi
    window="$lcontent"
    scan_line=$fline
    interrogated=0
    while :; do
      if [[ "$window" =~ \.(success|failure|stdout|stderr)\( ]]; then
        interrogated=1
        break
      fi
      [[ "$window" == *';'* ]] && break
      scan_line=$((scan_line + 1))
      [[ $scan_line -gt $total_lines ]] && break
      window="$(sed -n "${scan_line}p" "$file")"
    done
    if [[ $interrogated -eq 0 ]]; then
      printf '%s:%s:VACUOUS: .assert() with no .success()/.failure()/.stdout()/.stderr() chained\n' "$file" "$fline" >&2
      fail=1
    fi
  done < <(grep -n '\.assert()' "$file" || true)
done < <(find "$dir" -maxdepth 1 -name '*.rs' -print0)

if [[ $fail -ne 0 ]]; then
  printf '::error::lint-acceptance-tests failed — see violations above\n' >&2
  exit 1
fi

printf 'lint-acceptance-tests: OK\n'

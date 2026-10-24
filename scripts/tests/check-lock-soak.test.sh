#!/usr/bin/env bash
# check-lock-soak.test.sh — Offline tests for scripts/check-lock-soak.sh.
#
# The crates.io lookup is replaced by JSON files in
# scripts/tests/fixtures/lock-soak/crates (RQMD_SOAK_FIXTURE_DIR), and the
# clock is pinned with RQMD_SOAK_NOW, so no network is used and the verdicts
# do not depend on the day the test runs.
#
# Usage: bash scripts/tests/check-lock-soak.test.sh
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="${here}/../check-lock-soak.sh"
fx="${here}/fixtures/lock-soak"
export RQMD_SOAK_NOW="2026-10-24T00:00:00Z"
export RQMD_SOAK_FIXTURE_DIR="${fx}/crates"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; }

# run_case <name> <expected-status> <expected-output-file> <args...>
run_case() {
  local name="$1" want_status="$2" want_out="$3" status=0
  shift 3
  bash "$script" "$@" > "$work/out" 2> "$work/err" || status=$?
  if [[ "$status" != "$want_status" ]]; then
    bad "$name: exit status $status, expected $want_status"
    cat "$work/out" "$work/err"
  elif ! diff -u "$want_out" "$work/out" > "$work/diff"; then
    bad "$name: output differs"
    cat "$work/diff"
  else
    ok "$name"
  fi
}

cat > "$work/verdicts" <<'EOF'
PASS  alpha 1.1.0 (published 2026-01-10, 286 days old)
FAIL  bravo 2.0.0 (published 2026-10-14, 9 days old, needs 30)
FAIL  echo 0.3.0 (yanked; published 2026-02-01)
FAIL  foxtrot 0.1.0 (crates.io lookup failed, cannot verify the release date)
SKIP  golf 0.2.0 (source is not crates.io: git+https://example.invalid/golf?rev=abc#abc)
PASS  hotel 0.1.0+build.5 (published 2026-09-24, 30 days old)
FAIL  india 0.1.0 (published 2026-09-24, 29 days old, needs 30)
PASS  juliet 1.0.0 (published 2026-09-23, 30 days old)
check-lock-soak: 8 changed, 4 failed, 1 skipped (minimum age 30 days)
EOF

# Gate mode: young, yanked and unverifiable crates fail the run. A crate that
# is exactly 30 days old passes (also when written with a UTC offset), one
# second short fails, a git source is skipped, an unchanged crate and a
# changed path crate are not checked, and [[patch.unused]] is ignored.
cp "$work/verdicts" "$work/want-gate"
run_case "gate mode reports every changed crate and exits 1" 1 "$work/want-gate" \
  "${fx}/old.lock" "${fx}/new.lock"

# Dry run: same verdicts, never fails.
{ cat "$work/verdicts"; echo "check-lock-soak: dry run, not failing"; } > "$work/want-dry"
run_case "--dry-run prints the same verdicts and exits 0" 0 "$work/want-dry" \
  --dry-run "${fx}/old.lock" "${fx}/new.lock"

# Flag position does not matter.
run_case "--dry-run may follow the lock paths" 0 "$work/want-dry" \
  "${fx}/old.lock" "${fx}/new.lock" --dry-run

# Only soaked crates changed.
cat > "$work/want-ok" <<'EOF'
PASS  alpha 1.1.0 (published 2026-01-10, 286 days old)
check-lock-soak: 1 changed, 0 failed, 0 skipped (minimum age 30 days)
EOF
run_case "a lock change that only adds a soaked crate passes" 0 "$work/want-ok" \
  "${fx}/old.lock" "${fx}/ok.lock"

# No change at all needs no lookup, so an empty fixture directory is enough.
mkdir "$work/empty"
echo "check-lock-soak: no crate versions added or changed" > "$work/want-none"
RQMD_SOAK_FIXTURE_DIR="$work/empty" \
  run_case "an unchanged lock passes without any lookup" 0 "$work/want-none" \
  "${fx}/new.lock" "${fx}/new.lock"

# Removing crates is not a new release, so nothing is checked.
echo "check-lock-soak: no crate versions added or changed" > "$work/want-none"
run_case "removing a crate passes" 0 "$work/want-none" \
  "${fx}/old.lock" "${fx}/removed.lock"

# RQMD_SOAK_DAYS moves the threshold: with 5 days bravo (9 days old) passes.
cat > "$work/want-days" <<'EOF'
PASS  alpha 1.1.0 (published 2026-01-10, 286 days old)
PASS  bravo 2.0.0 (published 2026-10-14, 9 days old)
FAIL  echo 0.3.0 (yanked; published 2026-02-01)
FAIL  foxtrot 0.1.0 (crates.io lookup failed, cannot verify the release date)
SKIP  golf 0.2.0 (source is not crates.io: git+https://example.invalid/golf?rev=abc#abc)
PASS  hotel 0.1.0+build.5 (published 2026-09-24, 30 days old)
PASS  india 0.1.0 (published 2026-09-24, 29 days old)
PASS  juliet 1.0.0 (published 2026-09-23, 30 days old)
check-lock-soak: 8 changed, 2 failed, 1 skipped (minimum age 5 days)
EOF
RQMD_SOAK_DAYS=5 \
  run_case "RQMD_SOAK_DAYS changes the minimum age" 1 "$work/want-days" \
  "${fx}/old.lock" "${fx}/new.lock"

# Usage errors exit 2 and print nothing on stdout.
: > "$work/want-empty"
run_case "no arguments is a usage error" 2 "$work/want-empty"
run_case "a missing lock file is a usage error" 2 "$work/want-empty" \
  "${fx}/old.lock" "${fx}/does-not-exist.lock"
run_case "an unknown option is a usage error" 2 "$work/want-empty" \
  --bogus "${fx}/old.lock" "${fx}/new.lock"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]

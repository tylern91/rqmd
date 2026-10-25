#!/usr/bin/env bash
# require-green-ci.test.sh — Offline tests for scripts/require-green-ci.sh.
#
# `gh` is replaced by a fake on PATH that prints the next JSON file of a
# scenario (the last one repeats) and records its arguments, so no network is
# used. The poll interval and timeout are shortened with environment
# variables. A throwaway git repository stands in for the checkout, so the
# "docs-only range" cases do not depend on this repository's history.
#
# Usage: bash scripts/tests/require-green-ci.test.sh
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="${here}/../require-green-ci.sh"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; }

# --- fake gh -----------------------------------------------------------------
mkdir -p "$work/bin"
cat > "$work/bin/gh" <<'EOF'
#!/usr/bin/env bash
# Fake gh: `gh run list ...` prints $FAKE_GH_DIR/<n>.json (n = call number,
# clamped to the last file). A file named <n>.fail makes that call exit 1.
set -Eeuo pipefail
printf '%s\n' "$*" >> "$FAKE_GH_DIR/calls.log"
[[ "$1 $2" == "run list" ]] || { echo "fake gh: unexpected: $*" >&2; exit 64; }
n=$(( $(wc -l < "$FAKE_GH_DIR/calls.log") ))
last="$(find "$FAKE_GH_DIR" -name '[0-9]*.json' -o -name '[0-9]*.fail' | sed 's#.*/##; s#\..*##' | sort -n | tail -n1)"
(( n > last )) && n="$last"
if [[ -e "$FAKE_GH_DIR/$n.fail" ]]; then
  echo "fake gh: HTTP 502" >&2
  exit 1
fi
cat "$FAKE_GH_DIR/$n.json"
EOF
chmod +x "$work/bin/gh"

# --- throwaway repository ----------------------------------------------------
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
repo="$work/repo"
git init -q "$repo"
(
  cd "$repo"
  mkdir -p crates/x docs
  echo a > crates/x/lib.rs
  echo a > docs/guide.md
  git add -A && git commit -q -m base && git tag v0.0.1
  echo b >> docs/guide.md
  git commit -q -am docs-only
  git rev-parse HEAD > "$work/docs-sha"
  echo b >> crates/x/lib.rs
  git commit -q -am code
  git rev-parse HEAD > "$work/code-sha"
)
docs_sha="$(cat "$work/docs-sha")"
code_sha="$(cat "$work/code-sha")"

# --- scenario helpers --------------------------------------------------------
# run <name>: JSON for one run. Fields: workflow status conclusion [created] [sha]
run_json() {
  local wf="$1" status="$2" conclusion="$3" created="${4:-2026-10-25T10:00:00Z}" sha="${5:-$SHA}"
  printf '{"databaseId":%s,"workflowName":"%s","status":"%s","conclusion":"%s","createdAt":"%s","headSha":"%s","url":"https://example.invalid/runs/%s"}' \
    "$RANDOM" "$wf" "$status" "$conclusion" "$created" "$sha" "$RANDOM"
}
# poll <n> <run-json>...: writes <n>.json holding the array of those runs.
poll() {
  local out="$work/gh/$1.json" first=1
  shift
  printf '[' > "$out"
  for r in "$@"; do
    [[ $first == 1 ]] || printf ',' >> "$out"
    printf '%s' "$r" >> "$out"
    first=0
  done
  printf ']\n' >> "$out"
}
new_scenario() { rm -rf "$work/gh"; mkdir -p "$work/gh"; : > "$work/gh/calls.log"; }
calls() { wc -l < "$work/gh/calls.log" | tr -d ' '; }
# calls_is <name> <n>: the fake gh was called exactly n times.
calls_is() { if [[ "$(calls)" == "$2" ]]; then ok "$1"; else bad "$1: gh called $(calls) times, expected $2"; fi; }
# calls_at_most <name> <n>
calls_at_most() { if (( $(calls) <= $2 )); then ok "$1"; else bad "$1: gh called $(calls) times, expected at most $2"; fi; }

RUST='Rust CI'
SEC='Security Scan'

# check <name> <expected-status> <expected-output-regex> <args...>
# Runs the script from inside the throwaway repo with a 1 s timeout and 1 s
# interval unless the case overrides them via CASE_TIMEOUT / CASE_INTERVAL.
check() {
  local name="$1" want_status="$2" want_re="$3" status=0
  shift 3
  (
    cd "$repo"
    PATH="$work/bin:$PATH" FAKE_GH_DIR="$work/gh" \
      RQMD_CI_WAIT_TIMEOUT="${CASE_TIMEOUT:-1}" RQMD_CI_WAIT_INTERVAL="${CASE_INTERVAL:-1}" \
      bash "$script" "$@"
  ) > "$work/out" 2>&1 || status=$?
  if [[ "$status" != "$want_status" ]]; then
    bad "$name: exit status $status, expected $want_status"
    cat "$work/out"
  elif ! grep -Eq -- "$want_re" "$work/out"; then
    bad "$name: output does not match /$want_re/"
    cat "$work/out"
  else
    ok "$name"
  fi
}

# 1. Both workflows succeeded on the commit: exit 0 after one poll.
SHA="$code_sha"
new_scenario
poll 1 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed success)"
check "success: both green" 0 'Rust CI.*success' "$SHA"
calls_is "success: polled once" 1
if grep -q -- "--commit $SHA" "$work/gh/calls.log"; then
  ok "success: queried the exact full sha"
else
  bad "success: gh was not given --commit <sha>"
fi

# 2. A failed workflow fails at once, without waiting for the other one.
new_scenario
poll 1 "$(run_json "$RUST" completed failure)" "$(run_json "$SEC" in_progress '')"
check "failure: red Rust CI" 1 'Rust CI.*failure' "$SHA"
calls_is "failure: did not keep polling" 1

new_scenario
poll 1 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed failure)"
check "failure: red Security Scan" 1 'Security Scan.*failure' "$SHA"

# 3. Still running, then success.
new_scenario
poll 1 "$(run_json "$RUST" in_progress '')" "$(run_json "$SEC" completed success)"
poll 2 "$(run_json "$RUST" queued '')" "$(run_json "$SEC" completed success)"
poll 3 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed success)"
CASE_TIMEOUT=30 CASE_INTERVAL=0 check "running then success" 0 'Rust CI.*success' "$SHA"
calls_is "running then success: polled 3 times" 3

# 4. Timeout: a run that never finishes fails after the deadline.
new_scenario
poll 1 "$(run_json "$RUST" in_progress '')" "$(run_json "$SEC" completed success)"
# SECONDS has whole-second granularity, so a 1 s deadline can pass during the
# first poll; 2 s guarantees at least a second poll.
CASE_TIMEOUT=2 check "timeout: run never finishes" 1 'timed out' "$SHA"
if (( $(calls) >= 2 )); then ok "timeout: kept polling until the deadline"; else bad "timeout: gh called $(calls) times"; fi

# 4b. Timeout 0 checks once and does not wait (used by a dry run).
new_scenario
poll 1 "$(run_json "$RUST" in_progress '')" "$(run_json "$SEC" completed success)"
CASE_TIMEOUT=0 check "timeout 0: single check" 1 'timed out' "$SHA"
calls_is "timeout 0: polled once" 1

# 5. Docs-only range since the base ref: Rust CI is not required, Security Scan is.
SHA="$docs_sha"
new_scenario
poll 1 "$(run_json "$SEC" completed success)"
check "docs-only range: Rust CI not required" 0 'Rust CI.*not required' "$SHA" v0.0.1
new_scenario
poll 1 "$(run_json "$SEC" in_progress '')"
check "docs-only range: Security Scan still required" 1 'timed out' "$SHA" v0.0.1
new_scenario
poll 1 "$(run_json "$RUST" completed failure)" "$(run_json "$SEC" completed success)"
check "docs-only range: a red Rust CI does not block" 0 'Rust CI.*not required' "$SHA" v0.0.1

# 5b. A range that touches crates/ requires Rust CI.
SHA="$code_sha"
new_scenario
poll 1 "$(run_json "$SEC" completed success)"
check "code range: Rust CI required, run missing" 1 "no run of Rust CI" "$SHA" v0.0.1
new_scenario
poll 1 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed success)"
check "code range: Rust CI required and green" 0 'Rust CI.*success' "$SHA" v0.0.1

# 5c. An unknown base ref fails closed: Rust CI is required.
new_scenario
poll 1 "$(run_json "$SEC" completed success)"
check "unknown base ref: Rust CI required" 1 "no run of Rust CI" "$SHA" no-such-tag
new_scenario
poll 1 "$(run_json "$SEC" completed success)"
check "option-like base ref is not passed to git" 1 "no run of Rust CI" "$SHA" --output=/dev/null

# 6. Missing run: no run of a required workflow ever appears.
new_scenario
poll 1 "$(run_json "$RUST" completed success)"
check "missing run: Security Scan never ran" 1 "no run of Security Scan" "$SHA"
new_scenario
poll 1
check "missing run: no runs at all" 1 "no run of" "$SHA"

# 7. Cancelled without a rerun fails; a rerun or a newer run decides instead.
new_scenario
poll 1 "$(run_json "$RUST" completed cancelled)" "$(run_json "$SEC" completed success)"
check "cancelled without rerun" 1 'Rust CI.*cancelled' "$SHA"

new_scenario
poll 1 "$(run_json "$RUST" completed cancelled 2026-10-25T10:00:00Z)" "$(run_json "$SEC" completed success)"
# The first poll sees the cancelled run and must fail: a cancelled run is only
# forgiven when something newer exists on the commit.
check "cancelled, rerun not started yet" 1 'Rust CI.*cancelled' "$SHA"

new_scenario
poll 1 "$(run_json "$RUST" completed cancelled 2026-10-25T10:00:00Z)" \
  "$(run_json "$RUST" completed success 2026-10-25T10:20:00Z)" "$(run_json "$SEC" completed success)"
check "cancelled, newer run succeeded" 0 'Rust CI.*success' "$SHA"

new_scenario
poll 1 "$(run_json "$RUST" completed success 2026-10-25T10:00:00Z)" \
  "$(run_json "$RUST" completed failure 2026-10-25T10:20:00Z)" "$(run_json "$SEC" completed success)"
check "newest run decides: older success, newer failure" 1 'Rust CI.*failure' "$SHA"

# 7b. A run recorded for another commit is ignored.
new_scenario
poll 1 "$(run_json "$RUST" completed success 2026-10-25T10:00:00Z 0000000000000000000000000000000000000000)" \
  "$(run_json "$SEC" completed success)"
check "run on another sha is ignored" 1 "no run of Rust CI" "$SHA"

# 7c. Other conclusions are not success.
for c in skipped timed_out neutral action_required startup_failure stale; do
  new_scenario
  poll 1 "$(run_json "$RUST" completed "$c")" "$(run_json "$SEC" completed success)"
  check "conclusion $c is a failure" 1 "Rust CI.*$c" "$SHA"
done

# 8. API errors: a transient error is retried, a persistent one fails.
new_scenario
: > "$work/gh/1.fail"
poll 2 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed success)"
CASE_TIMEOUT=30 CASE_INTERVAL=0 check "transient gh error is retried" 0 'Rust CI.*success' "$SHA"

new_scenario
: > "$work/gh/1.fail"
CASE_TIMEOUT=30 CASE_INTERVAL=0 check "persistent gh error fails" 1 'gh run list failed' "$SHA"
calls_at_most "persistent gh error: gave up quickly" 10

# 9. Input validation: only a full lowercase sha is accepted, nothing reaches gh.
# shellcheck disable=SC2016 # the literal $(...) is the point of the case
for badsha in "" "abc123" "${code_sha:0:39}" "$(printf %s "$code_sha" | tr '[:lower:]' '[:upper:]')" "${code_sha}0" 'HEAD' '$(touch /tmp/x)' "--repo=evil/evil" "${code_sha};id"; do
  new_scenario
  poll 1 "$(run_json "$RUST" completed success)" "$(run_json "$SEC" completed success)"
  check "invalid sha rejected: '${badsha:0:20}'" 2 'full 40-character' "$badsha"
  calls_is "invalid sha did not reach gh: '${badsha:0:20}'" 0
done
check "no arguments" 2 'Usage'

# 9b. RQMD_CI_LEVEL picks the annotation level; the exit status is unchanged.
new_scenario
poll 1 "$(run_json "$RUST" completed failure)" "$(run_json "$SEC" completed success)"
RQMD_CI_LEVEL=warning check "level warning: still exits 1" 1 '^::warning::CI is not green' "$SHA"
if grep -q '^::error::' "$work/out"; then bad "level warning: an error annotation was emitted"; else ok "level warning: no error annotation"; fi
RQMD_CI_LEVEL=loud check "invalid level rejected" 2 'RQMD_CI_LEVEL' "$SHA"

# 10. Output sanitation: a hostile URL cannot start a workflow command.
new_scenario
printf '[{"databaseId":1,"workflowName":"Rust CI","status":"completed","conclusion":"failure","createdAt":"2026-10-25T10:00:00Z","headSha":"%s","url":"\\n::set-output name=x::y\\n::error::forged"},%s]\n' \
  "$SHA" "$(run_json "$SEC" completed success)" > "$work/gh/1.json"
check "hostile run url" 1 'Rust CI.*failure' "$SHA"
if grep -Eq '^::(set-output|error::forged|add-mask|stop-commands)' "$work/out"; then
  bad "hostile run url: a workflow command reached the log"; cat "$work/out"
else
  ok "hostile run url: no workflow command reached the log"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" == 0 ]]

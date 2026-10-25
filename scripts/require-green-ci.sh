#!/usr/bin/env bash
# require-green-ci.sh — Wait until CI is green on one exact commit, or fail.
#
# Usage: require-green-ci.sh <full-commit-sha> [<base-ref>]
#
# `release.yml` runs this before it tags, so a release can no longer publish a
# commit whose CI is red, cancelled or still running. It polls
# `gh run list --commit <sha>` until the required workflows have concluded:
#
#   * `Security Scan` is always required. It has no path filter, so every push
#     to main produces a run.
#   * `Rust CI` is required only when the commits in <base-ref>..<sha> touch a
#     path that can change what it builds or tests (scripts/ci-rust-changed.sh
#     decides). A docs-only range skips it. Without <base-ref>, or when the
#     range cannot be computed, `Rust CI` is required: the check fails closed.
#
# For each required workflow the newest run on the commit decides. It passes on
# `success`. It fails on any other conclusion (failure, cancelled, skipped,
# timed_out, ...) as soon as it is seen, unless a newer run of the same
# workflow exists; a re-run of a cancelled run is the same run in progress
# again, so it is waited for. A required workflow with no run yet is waited for
# too, and fails at the deadline with "no run ... found".
#
# Exit status: 0 green, 1 not green (failed, cancelled, missing or timed out),
# 2 bad usage.
#
# Environment:
#   RQMD_CI_WAIT_TIMEOUT   seconds to wait (default 900). 0 checks once.
#   RQMD_CI_WAIT_INTERVAL  seconds between polls (default 20).
#   RQMD_CI_LEVEL          annotation level of the failure line: error
#                          (default) or warning (a dry run, which still exits 1).
#   GH_TOKEN               token for gh; `actions: read` is enough.
#   GH_REPO                repository for gh when not run inside a clone.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

rust_workflow='Rust CI'
security_workflow='Security Scan'
max_gh_failures=5

usage() {
  echo "Usage: require-green-ci.sh <full-commit-sha> [<base-ref>]" >&2
  exit 2
}

[[ $# -ge 1 && $# -le 2 ]] || usage
sha="$1"
base="${2:-}"

# gh only matches a full sha, and the value ends up in a command line and in
# the log, so accept nothing else.
if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "::error::the commit must be a full 40-character lowercase sha" >&2
  usage
fi

timeout="${RQMD_CI_WAIT_TIMEOUT:-900}"
interval="${RQMD_CI_WAIT_INTERVAL:-20}"
if [[ ! "$timeout" =~ ^[0-9]+$ || ! "$interval" =~ ^[0-9]+$ ]]; then
  echo "::error::RQMD_CI_WAIT_TIMEOUT and RQMD_CI_WAIT_INTERVAL must be whole seconds" >&2
  exit 2
fi

level="${RQMD_CI_LEVEL:-error}"
if [[ "$level" != "error" && "$level" != "warning" ]]; then
  echo "::error::RQMD_CI_LEVEL must be error or warning" >&2
  exit 2
fi

# --- which workflows are required -------------------------------------------
rust_required=true
rust_reason="no base ref given"
if [[ -n "$base" ]]; then
  if [[ "$base" == -* ]] || ! git rev-parse --verify --quiet "${base}^{commit}" > /dev/null; then
    rust_reason="base ref '${base//[^A-Za-z0-9._\/-]/?}' not found"
  elif ! changed="$(git diff --name-only --no-renames "$base" "$sha" -- 2> /dev/null)"; then
    rust_reason="cannot diff ${base} against the commit"
  elif [[ "$(printf '%s\n' "$changed" | bash "${here}/ci-rust-changed.sh")" == "false" ]]; then
    rust_required=false
    rust_reason="no commit since ${base} touches a Rust CI path"
  else
    rust_reason="commits since ${base} touch a Rust CI path"
  fi
fi

required=("$security_workflow")
if [[ "$rust_required" == true ]]; then
  required=("$rust_workflow" "$security_workflow")
  printf 'Required: %s (%s), %s\n' "$rust_workflow" "$rust_reason" "$security_workflow"
else
  printf 'Required: %s. %s not required (%s).\n' "$security_workflow" "$rust_workflow" "$rust_reason"
fi
printf 'Commit: %s\nWaiting up to %ss, polling every %ss.\n' "$sha" "$timeout" "$interval"

# --- polling -----------------------------------------------------------------
errfile="$(mktemp)"
trap 'rm -f "$errfile"' EXIT

# newest_run <json> <workflow>: prints "<status>\t<conclusion>\t<url>" for the
# newest run of the workflow on the commit, or "missing" when there is none.
# @tsv escapes tabs and newlines, so a hostile field cannot split the line.
newest_run() {
  printf '%s' "$1" | jq -r --arg wf "$2" --arg sha "$sha" '
    [.[] | select(.workflowName == $wf and .headSha == $sha)]
    | sort_by(.createdAt)
    | last
    | if . == null then "missing"
      else [.status // "", .conclusion // "", .url // ""] | @tsv end'
}

# word <value>: a status or conclusion word, or "unknown" if it is not one.
word() {
  if [[ "$1" =~ ^[a-z_]+$ ]]; then printf '%s' "$1"; else printf 'unknown'; fi
}

# link <url>: " (<url>)" for a plain https URL, nothing otherwise.
link() {
  if [[ "$1" =~ ^https://[A-Za-z0-9./_-]+$ ]]; then printf ' (%s)' "$1"; fi
}

deadline=$((SECONDS + timeout))
gh_failures=0
last_report=""

while :; do
  if json="$(gh run list --commit "$sha" --limit 100 \
      --json databaseId,workflowName,status,conclusion,createdAt,headSha,url 2> "$errfile")" \
      && printf '%s' "$json" | jq -e 'type == "array"' > /dev/null 2>&1; then
    gh_failures=0
    failed=false
    pending=false
    report=""
    waiting_on=""
    for wf in "${required[@]}"; do
      IFS=$'\t' read -r status conclusion url <<< "$(newest_run "$json" "$wf")" || true
      if [[ "$status" == "missing" ]]; then
        pending=true
        report+="[wait] ${wf}: no run of ${wf} found on this commit yet"$'\n'
        waiting_on+="no run of ${wf} found; "
        continue
      fi
      status="$(word "$status")"
      conclusion="$(word "$conclusion")"
      if [[ "$status" != "completed" ]]; then
        pending=true
        report+="[wait] ${wf}: ${status}$(link "$url")"$'\n'
        waiting_on+="${wf} is ${status}; "
      elif [[ "$conclusion" == "success" ]]; then
        report+="[ ok ] ${wf}: success$(link "$url")"$'\n'
      else
        failed=true
        report+="[FAIL] ${wf}: ${conclusion}$(link "$url")"$'\n'
      fi
    done

    # Print only when something changed, so a 15-minute wait stays readable.
    if [[ "$report" != "$last_report" ]]; then
      printf '%s' "$report"
      last_report="$report"
    fi

    if [[ "$failed" == true ]]; then
      echo "::${level}::CI is not green on ${sha}; refusing to release" >&2
      exit 1
    fi
    if [[ "$pending" == false ]]; then
      echo "CI is green on ${sha}."
      exit 0
    fi
  else
    gh_failures=$((gh_failures + 1))
    reason="$(head -n1 "$errfile" | tr -cd '[:print:]' | cut -c1-200)"
    echo "[warn] gh run list failed (${gh_failures}/${max_gh_failures}): ${reason:-no output}"
    if ((gh_failures >= max_gh_failures)); then
      echo "::${level}::gh run list failed ${max_gh_failures} times in a row; the token needs actions: read and the repository must be reachable" >&2
      exit 1
    fi
    waiting_on="gh run list failing; "
  fi

  if ((SECONDS >= deadline)); then
    echo "::${level}::timed out after ${timeout}s waiting for CI on ${sha}: ${waiting_on%; }" >&2
    exit 1
  fi
  remaining=$((deadline - SECONDS))
  sleep "$((interval < remaining ? interval : remaining))"
done

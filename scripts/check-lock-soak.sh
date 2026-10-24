#!/usr/bin/env bash
# check-lock-soak.sh — Fail when a Cargo.lock change pulls in a crate version
# that has not soaked on crates.io yet.
#
# Why: the dependency soak policy (CONTRIBUTING.md, "Dependency updates")
# allows a bump only to a stable release published at least 30 days ago, direct
# or transitive. A direct bump can drag in a young transitive crate, and a
# Dependabot PR does not look at the lock diff. This script turns the manual
# check into a gate: every crate version that appears in <new-lock> but not in
# <old-lock> must be published at least RQMD_SOAK_DAYS days (default 30) before
# now and must not be yanked, according to the crates.io API.
#
# Usage: check-lock-soak.sh [--dry-run] <old-lock> <new-lock>
#
#   --dry-run   Print the verdict for every changed crate, then exit 0 even
#               when some would fail. The CI job uses it when the PR carries
#               the `soak-waiver` label.
#
# Without --dry-run the exit status is 0 when every changed crate passes and 1
# when any crate fails, including a lookup that could not be completed (the
# gate fails closed). A usage error exits 2.
#
# Only crates from the crates.io registry are checked. A package with no
# `source` line (a workspace member or path dependency) is skipped silently.
# A package from any other source (git, another registry) is reported as
# SKIP, because crates.io says nothing about it, and does not fail the gate.
#
# Environment (all optional):
#   RQMD_SOAK_DAYS         Minimum age in days (default 30).
#   RQMD_SOAK_NOW          Reference time as an ISO 8601 UTC timestamp such as
#                          2026-10-24T00:00:00Z (default: the current time).
#                          Used by the tests to make the age deterministic.
#   RQMD_SOAK_FIXTURE_DIR  Read `<dir>/<name>-<version>.json` instead of
#                          calling crates.io. The file holds the body of
#                          GET /api/v1/crates/<name>/<version>. A missing file
#                          counts as a failed lookup. No network is used.
#   RQMD_SOAK_SLEEP        Seconds to wait between crates.io requests
#                          (default 1.1). crates.io's data access policy
#                          allows at most one request per second.
#   RQMD_SOAK_API          API base URL (default https://crates.io/api/v1).
#
# Requires: bash 3.2+, awk, jq, and curl unless RQMD_SOAK_FIXTURE_DIR is set.
set -Eeuo pipefail

min_days="${RQMD_SOAK_DAYS:-30}"
sleep_s="${RQMD_SOAK_SLEEP:-1.1}"
api="${RQMD_SOAK_API:-https://crates.io/api/v1}"
fixture_dir="${RQMD_SOAK_FIXTURE_DIR:-}"
user_agent="rqmd-lock-soak (+https://github.com/tylern91/rqmd)"

die_usage() {
  printf 'usage: check-lock-soak.sh [--dry-run] <old-lock> <new-lock>\n' >&2
  exit 2
}

dry_run=0
files=()
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -h|--help) die_usage ;;
    -*) printf 'check-lock-soak: unknown option %s\n' "$arg" >&2; die_usage ;;
    *) files+=("$arg") ;;
  esac
done
[[ ${#files[@]} -eq 2 ]] || die_usage
old_lock="${files[0]}"
new_lock="${files[1]}"

for f in "$old_lock" "$new_lock"; do
  if [[ ! -f "$f" ]]; then
    printf 'check-lock-soak: missing file %s\n' "$f" >&2
    exit 2
  fi
done
case "$min_days" in
  ''|*[!0-9]*) printf 'check-lock-soak: RQMD_SOAK_DAYS must be a whole number\n' >&2; exit 2 ;;
esac
command -v jq >/dev/null 2>&1 || { printf 'check-lock-soak: jq is required\n' >&2; exit 2; }
if [[ -z "$fixture_dir" ]]; then
  command -v curl >/dev/null 2>&1 || { printf 'check-lock-soak: curl is required\n' >&2; exit 2; }
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Print one "name<TAB>version<TAB>source" line per [[package]] that has a
# `source`, sorted and unique. Other tables (for example [[patch.unused]]) are
# not packages and are ignored.
list_packages() {
  awk '
    function flush() {
      if (active && name != "" && ver != "" && src != "") {
        printf "%s\t%s\t%s\n", name, ver, src
      }
      active = 0; name = ""; ver = ""; src = ""
    }
    /^\[/ { flush(); if ($0 == "[[package]]") { active = 1 }; next }
    active && /^name = "/    { name = $0; sub(/^name = "/, "", name); sub(/"$/, "", name) }
    active && /^version = "/ { ver = $0;  sub(/^version = "/, "", ver); sub(/"$/, "", ver) }
    active && /^source = "/  { src = $0;  sub(/^source = "/, "", src); sub(/"$/, "", src) }
    END { flush() }
  ' "$1" | LC_ALL=C sort -u
}

# Seconds since the epoch for an ISO 8601 timestamp such as
# 2026-03-04T05:06:07.123456+00:00 or 2026-03-04T05:06:07Z. Pure bash
# arithmetic so it behaves the same with BSD and GNU date.
to_epoch() {
  local ts="$1" y m d hh mm ss rest sign oh om offset=0
  y=$((10#${ts:0:4})); m=$((10#${ts:5:2})); d=$((10#${ts:8:2}))
  hh=$((10#${ts:11:2})); mm=$((10#${ts:14:2})); ss=$((10#${ts:17:2}))
  rest="${ts:19}"
  # Drop a fractional-seconds part, then read an optional +HH:MM / -HH:MM.
  case "$rest" in .*) rest="${rest#.}"; rest="${rest#"${rest%%[!0-9]*}"}" ;; esac
  case "$rest" in
    [+-][0-9][0-9]:[0-9][0-9])
      sign="${rest:0:1}"; oh=$((10#${rest:1:2})); om=$((10#${rest:4:2}))
      offset=$((oh * 3600 + om * 60))
      [[ "$sign" == "-" ]] && offset=$((-offset))
      ;;
  esac
  local yy=$y era yoe doy doe days mp
  if [[ $m -le 2 ]]; then yy=$((y - 1)); fi
  if [[ $yy -ge 0 ]]; then era=$((yy / 400)); else era=$(((yy - 399) / 400)); fi
  yoe=$((yy - era * 400))
  if [[ $m -gt 2 ]]; then mp=$((m - 3)); else mp=$((m + 9)); fi
  doy=$(((153 * mp + 2) / 5 + d - 1))
  doe=$((yoe * 365 + yoe / 4 - yoe / 100 + doy))
  days=$((era * 146097 + doe - 719468))
  printf '%s\n' $((days * 86400 + hh * 3600 + mm * 60 + ss - offset))
}

if [[ -n "${RQMD_SOAK_NOW:-}" ]]; then
  now_epoch="$(to_epoch "$RQMD_SOAK_NOW")"
else
  now_epoch="$(date -u +%s)"
fi
min_seconds=$((min_days * 86400))

# Print "<created_at><TAB><yanked>" for one crate version, or return 1.
lookup() {
  local name="$1" ver="$2" body
  if [[ -n "$fixture_dir" ]]; then
    local file="${fixture_dir}/${name}-${ver}.json"
    [[ -f "$file" ]] || return 1
    body="$(cat "$file")"
  else
    # `+` in build metadata (1.0.0+zstd.1.5) is percent-encoded in the path.
    body="$(curl -fsS --max-time 30 --retry 3 --retry-delay 2 \
      -A "$user_agent" -H 'Accept: application/json' \
      "${api}/crates/${name}/${ver//+/%2B}")" || return 1
  fi
  printf '%s' "$body" |
    jq -er '[.version.created_at, (.version.yanked | tostring)] | @tsv' 2>/dev/null
}

list_packages "$old_lock" > "$work/old.tsv"
list_packages "$new_lock" > "$work/new.tsv"
# Lines only in the new lock: added crates and changed versions or sources.
LC_ALL=C comm -13 "$work/old.tsv" "$work/new.tsv" > "$work/changed.tsv"

total=0; failed=0; skipped=0; looked_up=0
while IFS=$'\t' read -r name ver src; do
  [[ -n "$name" ]] || continue
  total=$((total + 1))
  case "$src" in
    "registry+https://github.com/rust-lang/crates.io-index"|"sparse+https://index.crates.io/") ;;
    *)
      printf 'SKIP  %s %s (source is not crates.io: %s)\n' "$name" "$ver" "$src"
      skipped=$((skipped + 1))
      continue
      ;;
  esac

  # One request per second at most, and only between real requests.
  if [[ -z "$fixture_dir" && $looked_up -gt 0 ]]; then sleep "$sleep_s"; fi
  looked_up=$((looked_up + 1))

  if ! info="$(lookup "$name" "$ver")"; then
    printf 'FAIL  %s %s (crates.io lookup failed, cannot verify the release date)\n' "$name" "$ver"
    failed=$((failed + 1))
    continue
  fi
  created="${info%%$'\t'*}"
  yanked="${info#*$'\t'}"
  published="${created:0:10}"

  if ! created_epoch="$(to_epoch "$created" 2>/dev/null)"; then
    printf 'FAIL  %s %s (unreadable created_at %s)\n' "$name" "$ver" "$created"
    failed=$((failed + 1))
    continue
  fi
  age_days=$(((now_epoch - created_epoch) / 86400))

  if [[ "$yanked" == "true" ]]; then
    printf 'FAIL  %s %s (yanked; published %s)\n' "$name" "$ver" "$published"
    failed=$((failed + 1))
  elif [[ $((now_epoch - created_epoch)) -lt $min_seconds ]]; then
    printf 'FAIL  %s %s (published %s, %d days old, needs %d)\n' \
      "$name" "$ver" "$published" "$age_days" "$min_days"
    failed=$((failed + 1))
  else
    printf 'PASS  %s %s (published %s, %d days old)\n' "$name" "$ver" "$published" "$age_days"
  fi
done < "$work/changed.tsv"

if [[ $total -eq 0 ]]; then
  printf 'check-lock-soak: no crate versions added or changed\n'
  exit 0
fi

printf 'check-lock-soak: %d changed, %d failed, %d skipped (minimum age %d days)\n' \
  "$total" "$failed" "$skipped" "$min_days"

if [[ $failed -gt 0 ]]; then
  if [[ $dry_run -eq 1 ]]; then
    printf 'check-lock-soak: dry run, not failing\n'
    exit 0
  fi
  exit 1
fi

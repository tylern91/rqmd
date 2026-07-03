#!/usr/bin/env bash
# bump-version.sh — Compute next SemVer tag from current latest and a bump label.
#
# Usage: bump-version.sh <latest_tag> <label>
# Example: bump-version.sh v2.0.0 minor  →  v2.1.0
# Output: vMAJOR.MINOR.PATCH on stdout
set -Eeuo pipefail

latest="${1:?Usage: bump-version.sh <latest_tag> <label>}"
label="${2:?Usage: bump-version.sh <latest_tag> <label>}"

if ! printf '%s' "$latest" | grep -qE '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
  printf 'bump-version: invalid tag format "%s" — expected vMAJOR.MINOR.PATCH\n' "$latest" >&2
  exit 1
fi

# Strip leading v, split on .
ver="${latest#v}"
maj="${ver%%.*}"; rest="${ver#*.}"
min="${rest%%.*}"; pat="${rest#*.}"

case "$label" in
  major) maj=$((maj + 1)); min=0; pat=0 ;;
  minor) min=$((min + 1)); pat=0 ;;
  patch) pat=$((pat + 1)) ;;
  *)
    printf 'bump-version: invalid label "%s" — expected major|minor|patch\n' "$label" >&2
    exit 1
    ;;
esac

printf 'v%s.%s.%s\n' "$maj" "$min" "$pat"

#!/usr/bin/env bash
# build-release-notes.sh — Build GitHub Release notes from CHANGELOG.md.
#
# Usage:
#   build-release-notes.sh <version> <label> <breaking> [--from-existing]
#
# Args:
#   version       — target version string (e.g. v2.1.0), used only with --from-existing
#   label         — major|minor|patch (informational only)
#   breaking      — true|false — prepend breaking-change callout when true
#   --from-existing — read the matching [version] section instead of [Unreleased]
#
# Environment:
#   CHANGELOG     — path to CHANGELOG.md (default: ./CHANGELOG.md)
#   PR_BODY       — raw PR body; if it contains a "## Migration" section, it is appended
#
# Output: release notes markdown on stdout
set -Eeuo pipefail

version="${1:-}"
label="${2:-patch}"
breaking="${3:-false}"
from_existing=false
[ "${4:-}" = "--from-existing" ] && from_existing=true

CHANGELOG="${CHANGELOG:-CHANGELOG.md}"

if [ ! -f "$CHANGELOG" ]; then
  printf 'build-release-notes: CHANGELOG not found at %s\n' "$CHANGELOG" >&2
  exit 1
fi

# Extract the relevant block using awk
if [ "$from_existing" = "true" ]; then
  # Strip leading v for matching inside CHANGELOG (e.g. v2.1.0 → 2.1.0)
  ver_bare="${version#v}"
  body=$(awk -v ver="$ver_bare" '
    /^## \[/ && index($0, "[" ver "]") { found=1; next }
    /^## \[/ && found { exit }
    found { print }
  ' "$CHANGELOG" | sed '/^[[:space:]]*$/{ N; /^\n$/d; }')
else
  body=$(awk '
    /^## \[Unreleased\]/ { found=1; next }
    /^## \[/ && found { exit }
    found { print }
  ' "$CHANGELOG" \
    | grep -v '^---$' \
    | awk 'NF{p=1} p')
fi

# Strip empty type-bucket headings (headings followed immediately by another heading or EOF)
body=$(printf '%s' "$body" | awk '
  /^### / { pending=$0; next }
  /^[[:space:]]*$/ { if (pending != "") { print ""; next } print; next }
  { if (pending != "") { print pending; pending="" } print }
  END { }
')

# Prepend breaking-change callout
if [ "$breaking" = "true" ]; then
  callout='> Warning: **Breaking Changes**
>
> Review the changes below carefully before upgrading.

'
  body="${callout}${body}"
fi

# Append Migration section from PR body if present
if [ -n "${PR_BODY:-}" ]; then
  migration=$(printf '%s' "$PR_BODY" | awk '/^## Migration/{found=1; next} /^## [^M]/{if(found) exit} found{print}')
  if [ -n "$migration" ]; then
    body="${body}

## Migration

${migration}"
  fi
fi

printf '%s\n' "$body"

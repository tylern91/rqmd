#!/usr/bin/env bash
# build-release-notes.sh — Build GitHub Release notes from CHANGELOG.md.
#
# Usage:
#   build-release-notes.sh <version> <label> <breaking> [--from-existing] [--prev <tag>]
#
# Args:
#   version       — target version string (e.g. v2.1.0), used only with --from-existing
#   label         — major|minor|patch (informational only)
#   breaking      — true|false — prepend breaking-change callout when true
#   --from-existing — read the matching [version] section instead of [Unreleased]
#   --prev <tag>  — previous release tag; when set, appends a "Full Changelog" compare
#                   link and an Install section with versioned asset URLs
#
# Environment:
#   CHANGELOG        — path to CHANGELOG.md (default: ./CHANGELOG.md)
#   PR_BODY          — raw PR body; if it contains a "## Migration" section, it is appended
#   REPO_SLUG        — owner/repo (default: $GITHUB_REPOSITORY, else derived from origin)
#   ASSET_SHA256_DARWIN — sha256 of the aarch64-apple-darwin tarball, if already known
#   ASSET_SHA256_LINUX  — sha256 of the x86_64-unknown-linux-gnu tarball, if already known
#
# Output: release notes markdown on stdout
set -Eeuo pipefail

version="${1:-}"
label="${2:-patch}"
breaking="${3:-false}"
from_existing=false
prev=""
shift 3 || true
while [ $# -gt 0 ]; do
  case "$1" in
    --from-existing) from_existing=true ;;
    --prev) shift; prev="${1:-}" ;;
  esac
  shift || true
done

CHANGELOG="${CHANGELOG:-CHANGELOG.md}"
if [ -z "${REPO_SLUG:-}" ]; then
  # Handles both "git@github.com:owner/repo.git" and "https://github.com/owner/repo.git" —
  # take the last two "/"-or-":"-delimited fields, whichever separator the URL used.
  origin_url="$(git remote get-url origin 2>/dev/null || true)"
  origin_url="${origin_url%.git}"
  REPO_SLUG="${GITHUB_REPOSITORY:-$(printf '%s' "$origin_url" | awk -F'[/:]' '{print $(NF-1)"/"$NF}')}"
fi

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
  ' "$CHANGELOG" \
    | grep -v '^---$' \
    | sed '/^[[:space:]]*$/{ N; /^\n$/d; }')
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

# Append Full Changelog compare link + Install section when the previous tag is known.
# --prev is only passed by callers that already have a concrete tag range (i.e. real
# releases); the [Unreleased] preview path has no "next" tag yet, so it's skipped there.
if [ -n "$prev" ] && [ -n "$REPO_SLUG" ]; then
  compare_range="${prev}...${version}"
  darwin_asset="rqmd-${version}-aarch64-apple-darwin.tar.gz"
  linux_asset="rqmd-${version}-x86_64-unknown-linux-gnu.tar.gz"
  darwin_sha="${ASSET_SHA256_DARWIN:-not yet available — verify against the .sha256 sidecar}"
  linux_sha="${ASSET_SHA256_LINUX:-not yet available — verify against the .sha256 sidecar}"

  body="${body}

## Install

\`\`\`sh
# macOS (Apple Silicon)
curl -fLO https://github.com/${REPO_SLUG}/releases/download/${version}/${darwin_asset}
curl -fLO https://github.com/${REPO_SLUG}/releases/download/${version}/${darwin_asset}.sha256
shasum -a 256 -c ${darwin_asset}.sha256
tar -xf ${darwin_asset}
install -m 0755 rqmd ~/.local/bin/rqmd

# Linux (x86_64)
curl -fLO https://github.com/${REPO_SLUG}/releases/download/${version}/${linux_asset}
curl -fLO https://github.com/${REPO_SLUG}/releases/download/${version}/${linux_asset}.sha256
shasum -a 256 -c ${linux_asset}.sha256
tar -xf ${linux_asset}
install -m 0755 rqmd ~/.local/bin/rqmd

# Homebrew
brew tap ${REPO_SLUG} && brew trust ${REPO_SLUG} && brew install rqmd
\`\`\`

| Asset | SHA-256 |
|---|---|
| \`${darwin_asset}\` | \`${darwin_sha}\` |
| \`${linux_asset}\` | \`${linux_sha}\` |

**Full Changelog**: [\`${compare_range}\`](https://github.com/${REPO_SLUG}/compare/${compare_range})"
fi

printf '%s\n' "$body"

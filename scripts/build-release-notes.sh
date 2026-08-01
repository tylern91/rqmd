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

# --from-existing is a flag, not a fixed positional slot — scan all args so
# it works in any position (release.yml passes it at $4, but callers should
# not have to match that exactly).
from_existing=false
positional=()
for arg in "$@"; do
  if [ "$arg" = "--from-existing" ]; then
    from_existing=true
  else
    positional+=("$arg")
  fi
done

version="${positional[0]:-}"
label="${positional[1]:-patch}"
breaking="${positional[2]:-false}"

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
    /^\[.*\]: https?:\/\// && found { exit }
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

# --- Provenance footer (commit / PR / full changelog) ---
# Emitted only when the extracted body is non-empty (so release.yml's
# empty-notes guard, which counts non-whitespace chars in this same output,
# keeps working). NOTES_FOOTER=0 disables it entirely.
if [ "${NOTES_FOOTER:-1}" != "0" ] && [ -n "$(printf '%s' "$body" | tr -d '[:space:]')" ]; then
  repo_slug="${GITHUB_REPOSITORY:-}"
  if [ -z "$repo_slug" ]; then
    origin_url=$(git remote get-url origin 2>/dev/null || true)
    repo_slug=$(printf '%s' "$origin_url" \
      | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')
  fi

  if [ -n "$repo_slug" ]; then
    if git rev-parse -q --verify "refs/tags/${version}" >/dev/null 2>&1; then
      commit_sha=$(git rev-list -n1 "$version" 2>/dev/null || true)
    else
      commit_sha=$(git rev-parse HEAD 2>/dev/null || true)
    fi

    pr_number="${PR_NUMBER:-}"
    if ! printf '%s' "$pr_number" | grep -qE '^[0-9]+$'; then
      pr_number=""
    fi
    if [ -z "$pr_number" ] && [ -n "${commit_sha:-}" ]; then
      pr_number=$(git log -1 --format=%s "$commit_sha" 2>/dev/null \
        | grep -oE '#[0-9]+' | tr -d '#' || true)
    fi

    # Ascending walk: stop exactly at $version so regenerating an old tag's
    # notes still finds the tag before it, not the repo's newest tag.
    prev_tag=$(git tag --sort=v:refname 2>/dev/null \
      | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
      | awk -v v="$version" '$0 == v { exit } { last = $0 } END { if (last) print last }' \
      || true)

    footer=""
    if [ -n "${commit_sha:-}" ]; then
      short_sha=${commit_sha:0:7}
      footer="${footer}- **Commit:** [\`${short_sha}\`](https://github.com/${repo_slug}/commit/${commit_sha})
"
    fi
    if [ -n "$pr_number" ]; then
      footer="${footer}- **Pull request:** [#${pr_number}](https://github.com/${repo_slug}/pull/${pr_number})
"
    fi
    if [ -n "$prev_tag" ]; then
      footer="${footer}- **Full changelog:** [\`${prev_tag}...${version}\`](https://github.com/${repo_slug}/compare/${prev_tag}...${version})"
    else
      footer="${footer}- **Full changelog:** [\`${version}\`](https://github.com/${repo_slug}/commits/${version})"
    fi

    if [ -n "$footer" ]; then
      body="${body}

---

${footer}"
    fi
  fi
fi

printf '%s\n' "$body"

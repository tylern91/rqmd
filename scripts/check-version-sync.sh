#!/usr/bin/env bash
# check-version-sync.sh — Assert the workspace version matches the CHANGELOG's
# most recently released heading.
#
# Why: crate manifests inherit `version.workspace = true` from the single
# `[workspace.package] version = "..."` line in the root Cargo.toml. Nothing
# forces that line to move in lockstep with CHANGELOG.md's finalized release
# heading (`## [Unreleased]` -> `## [X.Y.Z] - DATE`) — a forgotten bump ships
# a binary that reports the previous release's version (see CHANGELOG entry
# for v0.4.1 for the incident this guards against).
#
# Usage: check-version-sync.sh [path-to-repo-root]
set -Eeuo pipefail

root="${1:-.}"
cargo_toml="${root}/Cargo.toml"
changelog="${root}/CHANGELOG.md"

for f in "$cargo_toml" "$changelog"; do
  if [[ ! -f "$f" ]]; then
    printf 'check-version-sync: missing file %s\n' "$f" >&2
    exit 1
  fi
done

# Extract the version from [workspace.package] specifically — not any
# [package] version.workspace inheritance line, and not a dependency's
# pinned version that happens to say `version = "..."` earlier in the file.
workspace_version="$(awk '
  /^\[workspace\.package\]/ { in_section = 1; next }
  /^\[/ { in_section = 0 }
  in_section && /^version[[:space:]]*=/ {
    match($0, /"[^"]+"/)
    print substr($0, RSTART + 1, RLENGTH - 2)
    exit
  }
' "$cargo_toml")"

if [[ -z "$workspace_version" ]]; then
  printf 'check-version-sync: no [workspace.package] version found in %s\n' "$cargo_toml" >&2
  exit 1
fi

# First "## [X.Y.Z]" heading — skips "## [Unreleased]" since its bracket
# content isn't numeric.
changelog_version="$(grep -m1 -E '^## \[[0-9]+\.[0-9]+\.[0-9]+\]' "$changelog" | sed -E 's/^## \[([0-9]+\.[0-9]+\.[0-9]+)\].*/\1/')"

if [[ -z "$changelog_version" ]]; then
  printf 'check-version-sync: no released "## [X.Y.Z]" heading found in %s\n' "$changelog" >&2
  exit 1
fi

if [[ "$workspace_version" != "$changelog_version" ]]; then
  printf 'check-version-sync: MISMATCH — Cargo.toml [workspace.package] version is "%s" but CHANGELOG.md top release is "%s"\n' \
    "$workspace_version" "$changelog_version" >&2
  printf 'Bump [workspace.package] version in %s to match, or finalize CHANGELOG.md if it is stale.\n' "$cargo_toml" >&2
  exit 1
fi

printf 'check-version-sync: OK — %s == %s\n' "$workspace_version" "$changelog_version"

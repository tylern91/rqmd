#!/usr/bin/env bash
# stamp-changelog-refs.sh — append commit/PR/author provenance to CHANGELOG bullets.
#
# Usage:
#   stamp-changelog-refs.sh <bare-version>
#
# Args:
#   bare-version  — e.g. 0.8.0 (no leading "v"); must already exist as a
#                   "## [<version>]" heading in CHANGELOG.md and as tag v<version>.
#
# Environment:
#   CHANGELOG     — path to CHANGELOG.md (default: ./CHANGELOG.md)
#   REPO_SLUG     — owner/repo (default: derived from `git remote get-url origin`)
#
# Behavior:
#   - Every "- " bullet in the version's section gets a trailing ref:
#       ([`<hash>`](.../commit/<hash>)) by [@user](.../<user>) in [#NN](.../pull/NN)
#   - Bullets spanning multiple lines (continuation lines indented 2 spaces) are
#     stamped on their LAST physical line, never mid-sentence.
#   - Idempotent: a bullet whose last line already contains "/commit/" is left alone.
#   - Single-commit release (the common case): every bullet gets that commit's ref.
#   - Multi-commit release: each bullet's leading backtick token is matched against
#     commit scopes (`type(scope): ...`). Unique match wins. No match or ambiguous
#     match falls back to stamping ALL of the release's refs and prints the bullet
#     to stderr as needing manual review — this script never silently guesses.
#
# Output: rewrites CHANGELOG in place; review-needed bullets go to stderr.
set -Eeuo pipefail

version="${1:?usage: stamp-changelog-refs.sh <bare-version>}"
CHANGELOG="${CHANGELOG:-CHANGELOG.md}"
tag="v${version}"

if [ ! -f "$CHANGELOG" ]; then
  printf 'stamp-changelog-refs: CHANGELOG not found at %s\n' "$CHANGELOG" >&2
  exit 1
fi

# Normally the tag must already exist (we're backfilling a published release). The one
# exception is release.yml's own `release` job: it stamps CHANGELOG.md in its ephemeral
# checkout *before* the tag is created, so the body it builds from --from-existing carries
# refs. STAMP_HEAD_REF opts into that: point the range end at a ref that DOES exist (HEAD)
# instead of the not-yet-created tag. Unset/empty leaves today's exact behavior untouched.
range_end="$tag"
head_override=false
if ! git rev-parse "$tag" >/dev/null 2>&1; then
  if [ -n "${STAMP_HEAD_REF:-}" ] && git rev-parse "${STAMP_HEAD_REF}" >/dev/null 2>&1; then
    range_end="$STAMP_HEAD_REF"
    head_override=true
  else
    printf 'stamp-changelog-refs: tag %s not found\n' "$tag" >&2
    exit 1
  fi
fi

# Handles both "git@github.com:owner/repo.git" and "https://github.com/owner/repo.git" —
# take the last two "/"-or-":"-delimited fields, whichever separator the URL used.
if [ -z "${REPO_SLUG:-}" ]; then
  origin_url="$(git remote get-url origin 2>/dev/null || true)"
  origin_url="${origin_url%.git}"
  REPO_SLUG="$(printf '%s' "$origin_url" | awk -F'[/:]' '{print $(NF-1)"/"$NF}')"
fi

# --- Resolve the previous tag, and the commit range for this release --------
all_tags="$(git tag --sort=v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$')"
if [ "$head_override" = true ]; then
  # $tag doesn't exist in $all_tags yet (we're stamping pre-tag-creation) — the current
  # latest real tag is this release's predecessor by definition, so grep -B1 -x "$tag"
  # (which needs $tag present in the list to find its neighbor) can't be used here.
  prev_tag="$(printf '%s\n' "$all_tags" | tail -1)"
else
  prev_tag="$(printf '%s\n' "$all_tags" | grep -B1 -F -x "$tag" | head -1)"
  [ "$prev_tag" = "$tag" ] && prev_tag=""
fi

if [ -n "$prev_tag" ]; then
  range="${prev_tag}..${range_end}"
else
  range="$range_end"
fi

all_commit_lines=()
while IFS= read -r line || [ -n "$line" ]; do
  all_commit_lines+=("$line")
done < <(git log --reverse --pretty='%h%x09%s' "$range")

# `chore(release): ...` commits are tag/version housekeeping (e.g. consolidating a
# mis-cut version back into this one) — they never contribute CHANGELOG bullets, so
# including them in the ref pool would attach a meaningless link to every bullet in
# a release that otherwise has a clean single-commit or scope-matched attribution.
re_chore_release='^chore\(release\):'
commit_lines=()
# bash 3.2 (macOS system /bin/bash) throws "unbound variable" under `set -u`
# when expanding "${arr[@]}" on a declared-but-empty array — the
# "${arr[@]+"${arr[@]}"}" idiom expands to nothing instead of erroring in
# that case (e.g. a commit range with zero commits, such as the very first
# release), and is a no-op for non-empty arrays on any bash version.
for entry in "${all_commit_lines[@]+"${all_commit_lines[@]}"}"; do
  subject="${entry#*$'\t'}"
  if [[ "$subject" =~ $re_chore_release ]]; then
    continue
  fi
  commit_lines+=("$entry")
done
if [ "${#commit_lines[@]}" -eq 0 ]; then
  if [ "${#all_commit_lines[@]}" -gt 0 ]; then
    printf 'stamp-changelog-refs: every commit in %s is chore(release) — using the unfiltered pool\n' "$range" >&2
    commit_lines=("${all_commit_lines[@]}")
  else
    printf 'stamp-changelog-refs: no commits in %s — skipping stamping\n' "$range" >&2
    exit 0
  fi
fi

# bash 3.2 (macOS system /bin/bash) has no ${var,,} lowercasing operator, and its
# =~ parser mishandles literal parens written inline — both patterns below must
# be held in a variable first, then matched unquoted, or bash 3.2 throws a
# "syntax error in conditional expression" at parse time.
lc() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }
re_scope='^[a-z]+\(([^)]+)\)!?:'
re_pr='\(#([0-9]+)\)[[:space:]]*$'

declare -a c_hash c_scope c_pr c_ref
for entry in "${commit_lines[@]}"; do
  hash="${entry%%$'\t'*}"
  subject="${entry#*$'\t'}"

  scope=""
  if [[ "$subject" =~ $re_scope ]]; then
    scope="$(lc "${BASH_REMATCH[1]}")"
  fi

  pr=""
  if [[ "$subject" =~ $re_pr ]]; then
    pr="${BASH_REMATCH[1]}"
  fi

  author=""
  if [ -n "$pr" ]; then
    author="$(gh pr view "$pr" --repo "$REPO_SLUG" --json author -q .author.login 2>/dev/null || true)"
  fi

  ref="([\`${hash}\`](https://github.com/${REPO_SLUG}/commit/${hash}))"
  [ -n "$author" ] && ref="${ref} by [@${author}](https://github.com/${author})"
  [ -n "$pr" ] && ref="${ref} in [#${pr}](https://github.com/${REPO_SLUG}/pull/${pr})"

  c_hash+=("$hash")
  c_scope+=("$scope")
  c_pr+=("$pr")
  c_ref+=("$ref")
done

multi=false
[ "${#c_hash[@]}" -gt 1 ] && multi=true

all_refs_joined=""
for r in "${c_ref[@]}"; do
  all_refs_joined="${all_refs_joined:+${all_refs_joined} · }${r}"
done

resolve_ref() {
  local first_line="$1"
  if [ "$multi" = false ]; then
    printf '%s' "${c_ref[0]}"
    return
  fi
  local token=""
  local re_token='`([a-zA-Z0-9_./-]+)`'
  if [[ "$first_line" =~ $re_token ]]; then
    token="$(lc "${BASH_REMATCH[1]}")"
  fi
  local match_idx=-1 match_count=0
  if [ -n "$token" ]; then
    for i in "${!c_scope[@]}"; do
      if [ -n "${c_scope[$i]}" ] && [ "${c_scope[$i]}" = "$token" ]; then
        match_idx=$i
        match_count=$((match_count + 1))
      fi
    done
  fi
  if [ "$match_count" -eq 1 ]; then
    printf '%s' "${c_ref[$match_idx]}"
  else
    printf 'REVIEW: %s: %s\n' "$tag" "${first_line:0:80}" >&2
    printf '%s' "$all_refs_joined"
  fi
}

# --- Locate the version's section in CHANGELOG -------------------------------
sec_start="$(grep -n -F "## [${version}]" "$CHANGELOG" | head -1 | cut -d: -f1)"
if [ -z "$sec_start" ]; then
  printf 'stamp-changelog-refs: no "## [%s]" heading in %s\n' "$version" "$CHANGELOG" >&2
  exit 1
fi
total_lines="$(wc -l < "$CHANGELOG" | tr -d ' ')"
sec_end="$(awk -v start="$sec_start" -v total="$total_lines" '
  /^## \[/ && NR > start { print NR - 1; found=1; exit }
  END { if (!found) print total }
' "$CHANGELOG")"

section_lines=()
while IFS= read -r line || [ -n "$line" ]; do
  section_lines+=("$line")
done < <(sed -n "${sec_start},${sec_end}p" "$CHANGELOG")

out=()
i=0
n=${#section_lines[@]}
while (( i < n )); do
  line="${section_lines[$i]}"
  if [[ "$line" =~ ^-\  ]]; then
    bullet=("$line")
    j=$((i + 1))
    while (( j < n )); do
      nxt="${section_lines[$j]}"
      if [[ "$nxt" =~ ^-\  ]] || [[ "$nxt" =~ ^#+\  ]] || [[ "$nxt" == "---" ]] || [[ -z "$nxt" ]]; then
        break
      fi
      bullet+=("$nxt")
      j=$((j + 1))
    done
    last_idx=$(( ${#bullet[@]} - 1 ))
    if [[ "${bullet[$last_idx]}" == *"/commit/"* ]]; then
      out+=("${bullet[@]}")
    else
      ref="$(resolve_ref "${bullet[0]}")"
      bullet[$last_idx]="${bullet[$last_idx]} ${ref}"
      out+=("${bullet[@]}")
    fi
    i=$j
  else
    out+=("$line")
    i=$((i + 1))
  fi
done

{
  head -n "$((sec_start - 1))" "$CHANGELOG"
  printf '%s\n' "${out[@]}"
  tail -n "+$((sec_end + 1))" "$CHANGELOG"
} > "${CHANGELOG}.tmp"
mv "${CHANGELOG}.tmp" "$CHANGELOG"

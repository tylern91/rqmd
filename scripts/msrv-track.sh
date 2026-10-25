#!/usr/bin/env bash
# msrv-track.sh — Keep rust-version at the newest stable Rust minor.
#
# Why: CONTRIBUTING.md ("MSRV policy") makes the MSRV the newest stable Rust
# minor, because the Rust project patches only its newest stable release.
# .github/workflows/msrv-track.yml runs this script weekly. It never merges
# anything: it opens (or refreshes) one pull request that a person merges after
# CI, and clippy on the new stable, pass.
#
# Usage: msrv-track.sh detect | apply | publish
#
#   detect   Compare the newest stable minor from the Rust release channel with
#            `rust-version` in Cargo.toml. Prints the verdict and writes
#            current_minor, latest_minor and action to $GITHUB_OUTPUT, where
#            action is one of:
#              current        rust-version is already the newest stable minor
#              wait-nixpkgs   a newer minor exists but nixpkgs-unstable has no
#                             rust/1_<minor>.nix yet; nothing to do this week
#              bump           a newer minor exists and nixpkgs ships it
#            Exits 0 for all three. A network or parse failure exits 1, so a
#            broken source never looks like "nothing to do".
#
#   apply    Needs MSRV_TARGET_MINOR (for example 100). Runs
#            `nix flake update nixpkgs` and checks that the updated nixpkgs
#            provides a rustc of at least 1.<target>. If it does not, flake.lock
#            is restored, applied=false is written to $GITHUB_OUTPUT and the
#            exit status is 0. Otherwise every place that names the MSRV is
#            edited (see edit_sites) and applied=true is written. A place that
#            no longer matches fails the run, so the edit list cannot drift
#            silently from the docs and workflows.
#
#   publish  Needs MSRV_TARGET_MINOR, GH_TOKEN and GITHUB_REPOSITORY, and the
#            edits from `apply` in the working tree. Commits them to
#            chore/msrv-<minor>, pushes, and opens a pull request labelled
#            `minor`, or refreshes the open one. Writes result to
#            $GITHUB_OUTPUT:
#              created            new branch and pull request
#              updated            open pull request refreshed from a newer main
#              unchanged          the open branch already holds exactly this tree
#              skipped-edited     the branch has commits the tracker did not
#                                 make (for example a lint fix); left alone
#              skipped-closed     a pull request for this minor was closed
#                                 without merging; someone chose to skip it
#              skipped-other-pr   another tracker pull request is still open
#
# Every value that reaches a command line, a branch name, a file or an output
# is a validated number or a fixed word. Text from the network (the channel
# manifest) is only matched against a regular expression, never evaluated.
#
# Requires: bash 3.2+, git, curl, jq, awk, sed, base64, and nix (apply) or gh
# (publish).
set -Eeuo pipefail

stable_url="https://static.rust-lang.org/dist/channel-rust-stable.toml"
nixpkgs_rust_url="https://raw.githubusercontent.com/NixOS/nixpkgs/nixpkgs-unstable/pkgs/development/compilers/rust"
base_branch="main"
bot_name="github-actions[bot]"
bot_email="41898282+github-actions[bot]@users.noreply.github.com"

die() {
  printf '::error::msrv-track: %s\n' "$*" >&2
  exit 1
}

# note <message>: an annotation on the run page, plain text so it reads the
# same without colour.
note() { printf '::notice::%s\n' "$*"; }

# set_output <key> <value>: print it and, on a runner, add it to the step outputs.
set_output() {
  printf '%s=%s\n' "$1" "$2"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf '%s=%s\n' "$1" "$2" >> "$GITHUB_OUTPUT"
  fi
}

# summary <line>: one line of Markdown for the run summary page.
summary() {
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    printf '%s\n' "$*" >> "$GITHUB_STEP_SUMMARY"
  fi
}

minor_re='^[1-9][0-9]{0,3}$'

# cargo_minor: the minor of rust-version in Cargo.toml ("1.99" or "1.99.0" -> 99).
cargo_minor() {
  local minor
  minor="$(sed -n 's/^rust-version = "1\.\([0-9]\{1,4\}\)\(\.[0-9]\{1,4\}\)\{0,1\}"[[:space:]]*\(#.*\)\{0,1\}$/\1/p' Cargo.toml | sed -n '1p')"
  [[ "$minor" =~ $minor_re ]] || die "Cargo.toml has no rust-version = \"1.<minor>\" line"
  printf '%s' "$minor"
}

# target_minor: MSRV_TARGET_MINOR, which must be a plain number.
target_minor() {
  local t="${MSRV_TARGET_MINOR:-}"
  [[ "$t" =~ $minor_re ]] || die "MSRV_TARGET_MINOR must be a minor number such as 100"
  printf '%s' "$t"
}

# latest_minor: the newest stable minor, read from the release channel manifest.
latest_minor() {
  local toml line minor
  local re='^version[[:space:]]*=[[:space:]]*"1\.([0-9]{1,4})\.[0-9]{1,4}([^0-9]|$)'
  toml="$(curl --fail --silent --show-error --location --proto '=https' --retry 3 --max-time 30 "$stable_url")" \
    || die "could not download $stable_url"
  line="$(awk '/^\[pkg\.rust\][[:space:]]*$/ { f = 1; next } /^\[/ { f = 0 } f && /^version[[:space:]]*=/ { print; exit }' <<< "$toml")"
  [[ "$line" =~ $re ]] || die "the [pkg.rust] version in the channel manifest is not 1.<minor>.<patch>"
  minor="${BASH_REMATCH[1]}"
  [[ "$minor" =~ $minor_re ]] || die "the stable minor is not a plain number"
  printf '%s' "$minor"
}

# nixpkgs_has_rust <minor>: 0 when nixpkgs-unstable has rust/1_<minor>.nix, 1
# when it answers 404, and a failed run for any other answer.
nixpkgs_has_rust() {
  local code
  code="$(curl --silent --show-error --location --proto '=https' --retry 3 --max-time 30 \
    --output /dev/null --write-out '%{http_code}' "${nixpkgs_rust_url}/1_${1}.nix")" \
    || die "could not reach nixpkgs to look for rust/1_${1}.nix"
  case "$code" in
    200) return 0 ;;
    404) return 1 ;;
    *) die "nixpkgs answered HTTP $code for rust/1_${1}.nix" ;;
  esac
}

cmd_detect() {
  local current latest
  current="$(cargo_minor)"
  latest="$(latest_minor)"
  set_output current_minor "$current"
  set_output latest_minor "$latest"
  if (( 10#$latest <= 10#$current )); then
    set_output action current
    note "rust-version 1.${current} already covers the newest stable Rust, 1.${latest}. Nothing to do."
    summary "rust-version is 1.${current} and the newest stable Rust is 1.${latest}. Nothing to do."
  elif nixpkgs_has_rust "$latest"; then
    set_output action bump
    note "Stable Rust 1.${latest} is out (rust-version is 1.${current}) and nixpkgs-unstable ships it. Opening or refreshing the pull request."
    summary "Stable Rust 1.${latest} is out and nixpkgs-unstable ships it, so a pull request will be opened or refreshed."
  else
    set_output action wait-nixpkgs
    note "Stable Rust 1.${latest} is out (rust-version is 1.${current}), but nixpkgs-unstable has no rust/1_${latest}.nix yet. Nothing to do; this job tries again next week."
    summary "Stable Rust 1.${latest} is out, but nixpkgs-unstable has no rust/1_${latest}.nix yet. No pull request this week; the job tries again next week."
  fi
}

# replace_literal <file> <old> <new> <expected-count>: replace every literal
# occurrence of <old> and fail unless there were exactly <expected-count>.
replace_literal() {
  local file="$1" old="$2" new="$3" want="$4" content rest result count=0
  [[ -f "$file" ]] || die "$file is missing, so the edit list in scripts/msrv-track.sh is out of date"
  content="$(cat "$file"; printf x)"
  content="${content%x}"
  rest="$content"
  while [[ "$rest" == *"$old"* ]]; do
    count=$((count + 1))
    rest="${rest#*"$old"}"
  done
  [[ "$count" -eq "$want" ]] \
    || die "$file: expected $want occurrence(s) of '$old' but found $count; update the edit list in scripts/msrv-track.sh"
  # Unquoted on purpose: inside double quotes, bash 3.2 keeps the inner quotes
  # of the replacement as literal characters. An assignment does not split.
  result=${content//"$old"/"$new"}
  printf '%s' "$result" > "$file"
}

# edit_sites <current-minor> <target-minor>: every place that names the MSRV.
# Keep in step with "MSRV policy" in CONTRIBUTING.md.
edit_sites() {
  local o="1.$1" n="1.$2"
  replace_literal Cargo.toml "rust-version = \"$o\"" "rust-version = \"$n\"" 1
  replace_literal .github/workflows/rust.yml "- toolchain: \"$o\"" "- toolchain: \"$n\"" 1
  replace_literal .github/workflows/rust.yml "The MSRV ($o) leg" "The MSRV ($n) leg" 1
  replace_literal CONTRIBUTING.md "rust-version = \"$o\"" "rust-version = \"$n\"" 1
  replace_literal CONTRIBUTING.md "[\"stable\", \"$o\"]" "[\"stable\", \"$n\"]" 1
  replace_literal docs/INSTALL.md "Rust stable ≥$o (" "Rust stable ≥$n (" 1
  replace_literal docs/INSTALL.md "(≥$o, " "(≥$n, " 1
  replace_literal flake.nix "MSRV is $o" "MSRV is $n" 1
}

# edit_changelog <target-minor>: add the "Changed" line under [Unreleased].
edit_changelog() {
  local t="$1" tmp entry
  [[ -f CHANGELOG.md ]] || die "CHANGELOG.md is missing"
  entry="$(cat <<'EOF'
- The minimum supported Rust version is now 1.@T@, the newest stable release. Older Rust releases no
  longer receive upstream fixes. Homebrew installs are unaffected; `cargo install` and source builds
  need Rust 1.@T@ or newer. The Nix dev shell takes its `rustc` from an updated `nixpkgs` input.
EOF
)"
  entry="${entry//@T@/$t}"
  tmp="$(mktemp)"
  MSRV_ENTRY="$entry" MSRV_MARKER="minimum supported Rust version is now 1.${t}," awk '
    function emit(   i, ch, end, ins, first, have) {
      have = 0
      ch = 0
      for (i = 1; i <= n; i++) {
        if (index(u[i], marker) > 0) have = 1
        if (u[i] == "### Changed") ch = i
      }
      if (have) {
        for (i = 1; i <= n; i++) print u[i]
        return
      }
      if (ch) {
        end = n + 1
        for (i = ch + 1; i <= n; i++) if (u[i] ~ /^### /) { end = i; break }
        ins = end
        while (ins > ch + 1 && u[ins - 1] == "") ins--
        for (i = 1; i < ins; i++) print u[i]
        print entry
        for (i = ins; i <= n; i++) print u[i]
      } else {
        first = 1
        while (first <= n && u[first] == "") first++
        print ""
        print "### Changed"
        print entry
        print ""
        for (i = first; i <= n; i++) print u[i]
      }
    }
    BEGIN { entry = ENVIRON["MSRV_ENTRY"]; marker = ENVIRON["MSRV_MARKER"]; state = 0; n = 0 }
    state == 0 { print; if ($0 == "## [Unreleased]") state = 1; next }
    state == 1 && /^## \[/ { emit(); state = 2; print; next }
    state == 1 { u[++n] = $0; next }
    { print }
    END { if (state == 1) emit(); if (state == 0) exit 3 }
  ' CHANGELOG.md > "$tmp" || { rm -f "$tmp"; die "CHANGELOG.md has no '## [Unreleased]' heading"; }
  cat "$tmp" > CHANGELOG.md
  rm -f "$tmp"
}

cmd_apply() {
  local target current rustc_version nix_minor
  local ver_re='^1\.([0-9]{1,4})\.[0-9]{1,4}'
  target="$(target_minor)"
  current="$(cargo_minor)"
  (( 10#$target > 10#$current )) || die "target 1.${target} is not newer than rust-version 1.${current}"

  echo "::group::Update the nixpkgs input"
  nix flake update nixpkgs
  rustc_version="$(nix eval --raw --inputs-from . nixpkgs#rustc.version)"
  echo "::endgroup::"
  [[ "$rustc_version" =~ $ver_re ]] || die "nix reported a rustc version that is not 1.<minor>.<patch>"
  nix_minor="${BASH_REMATCH[1]}"
  if (( 10#$nix_minor < 10#$target )); then
    git restore --source=HEAD --worktree -- flake.lock
    set_output applied false
    note "nixpkgs-unstable provides rustc 1.${nix_minor}, below the target 1.${target}, so flake.lock was left as it was. Nothing to do; this job tries again next week."
    summary "nixpkgs-unstable provides rustc 1.${nix_minor}, below the target 1.${target}. No pull request this week; the job tries again next week."
    return 0
  fi

  echo "::group::Edit the places that name the MSRV"
  edit_sites "$current" "$target"
  edit_changelog "$target"
  echo "::endgroup::"
  set_output applied true
  note "Raised the MSRV from 1.${current} to 1.${target} in the working tree."
}

# git_auth <git args...>: run git with the token as an HTTP header for this one
# command, so nothing is written to .git/config or to the log.
git_auth() {
  local cred
  cred="$(printf 'x-access-token:%s' "$GH_TOKEN" | base64 | tr -d '\n')"
  GIT_CONFIG_COUNT=1 \
    GIT_CONFIG_KEY_0="http.https://github.com/.extraheader" \
    GIT_CONFIG_VALUE_0="AUTHORIZATION: basic ${cred}" \
    git "$@"
}

pr_body() {
  cat <<EOF
## Summary

Raises the minimum supported Rust version to 1.${1}, the newest stable release. \`.github/workflows/msrv-track.yml\` opened this pull request after its weekly check found the new stable release and nixpkgs-unstable shipping it.

It edits \`Cargo.toml\`, the pinned MSRV leg in \`rust.yml\`, \`CONTRIBUTING.md\`, \`docs/INSTALL.md\`, the \`flake.nix\` comment and \`CHANGELOG.md\`, and updates \`flake.lock\` so the Nix dev shell's \`rustc\` meets the new floor. The tracker does not merge automatically.

Any lint the new compiler reports belongs in this pull request. The tracker refreshes the branch only while every commit on it is its own, so a fix pushed here is never overwritten.

## Test plan

- [ ] \`Rust CI\` is green, including clippy on the new stable
- [ ] \`Nix\` is green on both legs
- [ ] \`Security Scan\` is green

Merging this pull request cuts a minor release (label \`minor\`). To skip this version, close the pull request without merging: the tracker then leaves 1.${1} alone and picks up the next stable release.
EOF
}

cmd_publish() {
  local target repo branch title body open_list head_list other open_number remote_sha others result
  local files=(Cargo.toml .github/workflows/rust.yml CONTRIBUTING.md docs/INSTALL.md flake.nix flake.lock CHANGELOG.md)
  target="$(target_minor)"
  [[ -n "${GH_TOKEN:-}" ]] || die "GH_TOKEN is not set"
  repo="${GITHUB_REPOSITORY:-}"
  [[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "GITHUB_REPOSITORY must look like owner/name"
  branch="chore/msrv-${target}"
  title="chore: raise the minimum supported Rust version to 1.${target}"

  # One tracker pull request at a time.
  open_list="$(gh pr list --repo "$repo" --state open --limit 100 --json number,headRefName)" \
    || die "could not list open pull requests"
  other="$(jq -r --arg b "$branch" '[.[] | select(.headRefName | startswith("chore/msrv-")) | select(.headRefName != $b)] | length' <<< "$open_list")"
  [[ "$other" =~ ^[0-9]+$ ]] || die "could not read the open pull request list"
  if (( other > 0 )); then
    set_output result skipped-other-pr
    note "Another MSRV pull request is still open. Not opening one for 1.${target} until it is merged or closed."
    summary "Another MSRV pull request is still open, so none was opened for 1.${target}."
    return 0
  fi

  # A pull request for this branch that was closed without merging means a
  # person chose to skip this version.
  head_list="$(gh pr list --repo "$repo" --head "$branch" --state all --limit 20 --json number,state)" \
    || die "could not list pull requests for $branch"
  if [[ "$(jq -r '[.[] | select(.state != "OPEN")] | length' <<< "$head_list")" != 0 ]]; then
    set_output result skipped-closed
    note "A pull request for $branch was closed without merging. Leaving 1.${target} alone; the next stable release gets its own branch."
    summary "A pull request for $branch was closed without merging, so 1.${target} is skipped."
    return 0
  fi
  open_number="$(jq -r '[.[] | select(.state == "OPEN")][0].number // empty' <<< "$head_list")"
  if [[ -n "$open_number" && ! "$open_number" =~ ^[0-9]+$ ]]; then
    die "the pull request number is not a plain number"
  fi

  git checkout -q -B "$branch"
  git add -- "${files[@]}"
  if git diff --cached --quiet; then
    die "nothing to commit: run 'apply' first"
  fi
  git -c "user.name=$bot_name" -c "user.email=$bot_email" commit -q \
    -m "$title" \
    -m "Opened by the MSRV tracker (.github/workflows/msrv-track.yml)."

  result=created
  remote_sha="$(git_auth ls-remote --heads origin "refs/heads/$branch" | awk '{ print $1 }')"
  if [[ -n "$remote_sha" ]]; then
    git_auth fetch -q --no-tags origin "+refs/heads/${base_branch}:refs/remotes/origin/${base_branch}"
    git_auth fetch -q --no-tags origin "+refs/heads/${branch}:refs/remotes/origin/${branch}"
    others="$(git log --format=%ae "origin/${base_branch}..origin/${branch}" | grep -vxF "$bot_email" || true)"
    if [[ -n "$others" ]]; then
      set_output result skipped-edited
      note "$branch has commits the tracker did not make, so it was left as it is."
      summary "$branch has commits the tracker did not make, so it was left as it is."
      return 0
    fi
    if [[ "$(git rev-parse "origin/${branch}^{tree}")" == "$(git rev-parse 'HEAD^{tree}')" ]]; then
      result=unchanged
    else
      git_auth push -q --force-with-lease="refs/heads/${branch}:${remote_sha}" origin "HEAD:refs/heads/${branch}"
      result=updated
    fi
  else
    git_auth push -q origin "HEAD:refs/heads/${branch}"
  fi

  body="$(mktemp)"
  pr_body "$target" > "$body"
  if [[ -n "$open_number" ]]; then
    if [[ "$result" != unchanged ]]; then
      gh pr edit "$open_number" --repo "$repo" --body-file "$body" --add-label minor
    fi
  else
    result=created
    gh pr create --repo "$repo" --base "$base_branch" --head "$branch" \
      --title "$title" --body-file "$body" --label minor
  fi
  rm -f "$body"
  set_output result "$result"
  note "MSRV pull request for 1.${target}: ${result}."
  summary "MSRV pull request for 1.${target}: ${result}."
}

main() {
  local cmd="${1:-}"
  cd "$(git rev-parse --show-toplevel)"
  case "$cmd" in
    detect) cmd_detect ;;
    apply) cmd_apply ;;
    publish) cmd_publish ;;
    *)
      printf 'usage: msrv-track.sh detect | apply | publish\n' >&2
      exit 2
      ;;
  esac
}

main "$@"

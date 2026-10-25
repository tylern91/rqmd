#!/usr/bin/env bash
# msrv-track.test.sh — Offline tests for scripts/msrv-track.sh.
#
# curl, gh and nix are replaced by fakes on PATH that read fixtures and log
# their arguments, git runs for real against a local bare "origin", and the
# user's own git configuration is hidden, so no network is used and the
# verdicts do not depend on the day or the machine.
#
# Usage: bash scripts/tests/msrv-track.test.sh
#
# shellcheck disable=SC2016  # literal backticks and $( ) in single quotes are test data
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
script="$root/scripts/msrv-track.sh"
fx="$here/fixtures/msrv-track"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Hide the user's git configuration (identity, signing) from the script.
export GIT_CONFIG_GLOBAL="$work/gitconfig"
export GIT_CONFIG_NOSYSTEM=1
printf '[user]\n\tname = Test\n\temail = test@example.invalid\n[init]\n\tdefaultBranch = main\n' > "$GIT_CONFIG_GLOBAL"

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; }

# check <name> <command...>: ok when the command succeeds.
check() {
  local name="$1"
  shift
  if "$@" > "$work/check.out" 2>&1; then ok "$name"; else bad "$name"; cat "$work/check.out"; fi
}
# check_not <name> <command...>: ok when the command fails.
check_not() {
  local name="$1"
  shift
  if "$@" > "$work/check.out" 2>&1; then bad "$name"; cat "$work/check.out"; else ok "$name"; fi
}

# ---- fakes -----------------------------------------------------------------
bin="$work/bin"
mkdir "$bin"

cat > "$bin/curl" <<'EOF'
#!/usr/bin/env bash
# Fake curl: serves FAKE_STABLE_TOML for the stable channel manifest and the
# status FAKE_NIXPKGS_STATUS for a nixpkgs rust/1_<minor>.nix probe.
url=""
want_code=0
for a in "$@"; do
  [[ "$a" == "-w" || "$a" == "--write-out" ]] && want_code=1
  url="$a"
done
printf '%s\n' "$url" >> "$FAKE_CURL_LOG"
case "$url" in
  *channel-rust-stable.toml)
    [[ -n "${FAKE_STABLE_FAIL:-}" ]] && exit 22
    cat "$FAKE_STABLE_TOML" ;;
  */rust/1_*.nix)
    if [[ "$want_code" == 1 ]]; then printf '%s' "${FAKE_NIXPKGS_STATUS:-200}"; exit 0; fi
    exit 22 ;;
  *) exit 22 ;;
esac
EOF

cat > "$bin/nix" <<'EOF'
#!/usr/bin/env bash
# Fake nix: `flake update` rewrites flake.lock, `eval` prints FAKE_NIX_RUSTC.
printf '%s\n' "$*" >> "$FAKE_NIX_LOG"
case "$*" in
  "flake update nixpkgs") printf '{"nodes":"updated"}\n' > flake.lock ;;
  "eval --raw --inputs-from . nixpkgs#rustc.version") printf '%s' "${FAKE_NIX_RUSTC:-1.100.0}" ;;
  *) echo "fake nix: unexpected arguments: $*" >&2; exit 2 ;;
esac
EOF

cat > "$bin/gh" <<'EOF'
#!/usr/bin/env bash
# Fake gh: serves PR listings from files and records every call.
printf '%s\n' "$*" >> "$FAKE_GH_LOG"
save_body() {
  local i j
  for ((i = 1; i <= $#; i++)); do
    if [[ "${!i}" == "--body-file" ]]; then j=$((i + 1)); cp "${!j}" "$FAKE_GH_BODY"; fi
  done
}
case "$*" in
  "pr list "*"--state open"*) cat "$FAKE_GH_OPEN" ;;
  "pr list "*"--head "*) cat "$FAKE_GH_HEAD" ;;
  "pr create "*) save_body "$@"; echo "https://github.com/example/repo/pull/7" ;;
  "pr edit "*) save_body "$@" ;;
  *) echo "fake gh: unexpected arguments: $*" >&2; exit 2 ;;
esac
EOF
chmod +x "$bin/curl" "$bin/nix" "$bin/gh"
export PATH="$bin:$PATH"

# ---- sandbox repo ------------------------------------------------------------
# Minimal files that carry the version at every site the tracker edits.
write_tree() {
  mkdir -p .github/workflows docs
  cat > Cargo.toml <<'EOF'
[workspace.package]
version = "0.29.0"
edition = "2024"
rust-version = "1.99"
EOF
  cat > .github/workflows/rust.yml <<'EOF'
jobs:
  build-linux:
    # The MSRV (1.99) leg is a drift check, not a release gate.
    strategy:
      matrix:
        include:
          - toolchain: stable
          - toolchain: "1.99"
EOF
  cat > CONTRIBUTING.md <<'EOF'
## MSRV policy

`rust-version = "1.99"` in the root `Cargo.toml` is the newest stable minor.
- CI also runs a `["stable", "1.99"]` toolchain matrix.
EOF
  cat > docs/INSTALL.md <<'EOF'
Requires Rust stable ≥1.99 (the MSRV, enforced by a pinned CI job), cmake ≥3.14.
Requirements: Rust stable (≥1.99, the MSRV, enforced by a pinned CI job), cmake.
EOF
  cat > flake.nix <<'EOF'
        # rustc/cargo come from nixpkgs-unstable (unpinned) — MSRV is 1.99
        # (Cargo.toml [workspace.package].rust-version); drift matters.
EOF
  printf '{"nodes":"original"}\n' > flake.lock
  printf '# Changelog\n\n## [Unreleased]\n\n## [0.29.0] - 2026-10-25\n\n### Added\n- old entry\n' > CHANGELOG.md
}

# new_sandbox <name>: a bare origin plus a clone holding write_tree's files,
# with every fake pointed at fresh logs. Leaves the shell inside the clone.
new_sandbox() {
  sb="$work/$1"
  mkdir -p "$sb"
  git init -q --bare "$sb/origin.git"
  git clone -q "$sb/origin.git" "$sb/repo" 2> /dev/null
  cd "$sb/repo"
  write_tree
  git add -A
  git commit -q -m "initial"
  git push -q origin main 2> /dev/null
  export FAKE_CURL_LOG="$sb/curl.log" FAKE_NIX_LOG="$sb/nix.log" FAKE_GH_LOG="$sb/gh.log" FAKE_GH_BODY="$sb/body.md"
  : > "$FAKE_CURL_LOG"
  : > "$FAKE_NIX_LOG"
  : > "$FAKE_GH_LOG"
  export FAKE_GH_OPEN="$sb/open.json" FAKE_GH_HEAD="$sb/head.json"
  echo '[]' > "$FAKE_GH_OPEN"
  echo '[]' > "$FAKE_GH_HEAD"
  export GITHUB_OUTPUT="$sb/output"
  : > "$GITHUB_OUTPUT"
  unset FAKE_STABLE_FAIL FAKE_NIXPKGS_STATUS FAKE_NIX_RUSTC MSRV_TARGET_MINOR || true
}

# set_stable <version string as it appears in the manifest>
set_stable() {
  FAKE_STABLE_TOML="$sb/stable.toml"
  export FAKE_STABLE_TOML
  sed "s|@VERSION@|$1|" "$fx/channel-rust-stable.toml" > "$FAKE_STABLE_TOML"
}

# out_has <key> <value>: the step output file holds the line key=value.
out_has() { grep -qxF "$1=$2" "$GITHUB_OUTPUT"; }
run_script() { bash "$script" "$@" > "$sb/stdout" 2> "$sb/stderr"; }
unreleased() { awk '/^## \[Unreleased\]/{u=1;next} /^## \[/{u=0} u' CHANGELOG.md; }
changelog_entry_added() {
  unreleased | grep -qxF '### Changed' &&
    unreleased | grep -qF 'minimum supported Rust version is now 1.100'
}

# ===========================================================================
# detect
# ===========================================================================
new_sandbox detect-current
set_stable "1.99.1 (58cf8f9c3 2026-10-13)"
check "detect: stable minor equal to rust-version needs no action" run_script detect
check "detect: reports action=current" out_has action current
check "detect: reports the newest minor it saw" out_has latest_minor 99
check "detect: probes only the manifest when nothing is newer" test "$(wc -l < "$FAKE_CURL_LOG" | tr -d ' ')" = 1

new_sandbox detect-older
set_stable "1.98.2 (aaaaaaaaa 2026-08-20)"
check "detect: a stable older than rust-version succeeds" run_script detect
check "detect: an older stable is not a bump" out_has action current

new_sandbox detect-patch-form
sed -i.bak 's/^rust-version = "1.99"/rust-version = "1.99.0"/' Cargo.toml
rm Cargo.toml.bak
set_stable "1.99.1 (58cf8f9c3 2026-10-13)"
check "detect: rust-version written as 1.N.P compares by minor" run_script detect
check "detect: 1.N.P form reports action=current" out_has action current

new_sandbox detect-wait
set_stable "1.100.0 (bbbbbbbbb 2026-11-12)"
export FAKE_NIXPKGS_STATUS=404
check "detect: newer stable but nixpkgs lacks the toolchain exits 0" run_script detect
check "detect: reports action=wait-nixpkgs" out_has action wait-nixpkgs
check "detect: probes rust/1_100.nix" grep -qF '/pkgs/development/compilers/rust/1_100.nix' "$FAKE_CURL_LOG"
check "detect: says it will retry next week" grep -qi 'next week' "$sb/stdout"

new_sandbox detect-bump
set_stable "1.100.0 (bbbbbbbbb 2026-11-12)"
check "detect: newer stable with nixpkgs ready exits 0" run_script detect
check "detect: reports action=bump" out_has action bump
check "detect: reports the target minor" out_has latest_minor 100
check "detect: reports the current minor" out_has current_minor 99

new_sandbox detect-skip-many
set_stable "1.102.0 (ccccccccc 2027-01-01)"
check "detect: a jump of several minors succeeds" run_script detect
check "detect: a jump of several minors targets the newest" grep -qF '/1_102.nix' "$FAKE_CURL_LOG"

new_sandbox detect-nix-error
set_stable "1.100.0 (bbbbbbbbb 2026-11-12)"
export FAKE_NIXPKGS_STATUS=503
check_not "detect: a nixpkgs error other than 404 fails the run" run_script detect
check_not "detect: a nixpkgs error is not reported as wait" grep -qx 'action=wait-nixpkgs' "$GITHUB_OUTPUT"

new_sandbox detect-fetch-error
export FAKE_STABLE_FAIL=1
check_not "detect: an unreachable manifest fails the run" run_script detect

new_sandbox detect-malformed
set_stable "nightly"
check_not "detect: a manifest version that is not 1.N.P fails the run" run_script detect
set_stable "1.1x0.0 (a 2026-01-01)"
check_not "detect: a non-numeric minor fails the run" run_script detect
set_stable "2.0.0 (a 2026-01-01)"
check_not "detect: another major version fails the run" run_script detect

new_sandbox detect-hostile
set_stable "1.100.0 (\$(touch $sb/PWNED) \`touch $sb/PWNED2\`)"
export FAKE_NIXPKGS_STATUS=404
check "detect: text after the version is ignored" run_script detect
check_not "detect: hostile manifest text is never executed (command substitution)" test -e "$sb/PWNED"
check_not "detect: hostile manifest text is never executed (backticks)" test -e "$sb/PWNED2"
check_not "detect: hostile manifest text is never written to the outputs" grep -q 'PWNED' "$GITHUB_OUTPUT"

new_sandbox detect-bad-cargo
set_stable "1.100.0 (bbbbbbbbb 2026-11-12)"
sed -i.bak '/^rust-version/d' Cargo.toml
rm Cargo.toml.bak
check_not "detect: a Cargo.toml without rust-version fails the run" run_script detect

# ===========================================================================
# apply
# ===========================================================================
new_sandbox apply-ok
export MSRV_TARGET_MINOR=100
check "apply: edits every site" run_script apply
check "apply: reports applied=true" out_has applied true
check "apply: Cargo.toml" grep -qxF 'rust-version = "1.100"' Cargo.toml
check "apply: MSRV matrix leg" grep -qxF '          - toolchain: "1.100"' .github/workflows/rust.yml
check "apply: stable leg is untouched" grep -qxF '          - toolchain: stable' .github/workflows/rust.yml
check "apply: rust.yml comment" grep -qF 'The MSRV (1.100) leg' .github/workflows/rust.yml
check "apply: CONTRIBUTING rust-version line" grep -qF '`rust-version = "1.100"`' CONTRIBUTING.md
check "apply: CONTRIBUTING matrix line" grep -qF '`["stable", "1.100"]`' CONTRIBUTING.md
check "apply: INSTALL has both lines" test "$(grep -cF '≥1.100' docs/INSTALL.md)" = 2
check "apply: flake.nix comment" grep -qF 'MSRV is 1.100' flake.nix
check_not "apply: no 1.99 left in the edited files" grep -n -e '1\.99' Cargo.toml .github/workflows/rust.yml CONTRIBUTING.md docs/INSTALL.md flake.nix
check "apply: flake.lock is the updated one" grep -qF '"updated"' flake.lock
check "apply: asks nix only to update nixpkgs" grep -qxF 'flake update nixpkgs' "$FAKE_NIX_LOG"
check "apply: CHANGELOG gets a Changed entry under Unreleased" changelog_entry_added
check "apply: CHANGELOG history is untouched" grep -qxF -- '- old entry' CHANGELOG.md

new_sandbox apply-nix-behind
export MSRV_TARGET_MINOR=100
export FAKE_NIX_RUSTC="1.99.0"
check "apply: a nixpkgs rustc below the target is not an error" run_script apply
check "apply: reports applied=false" out_has applied false
check "apply: flake.lock is restored" grep -qxF '{"nodes":"original"}' flake.lock
check "apply: no file is edited" git diff --quiet
check "apply: says it will retry next week" grep -qi 'next week' "$sb/stdout"

new_sandbox apply-nix-patch-ok
export MSRV_TARGET_MINOR=100
export FAKE_NIX_RUSTC="1.100.3"
check "apply: a nixpkgs rustc 1.100.3 meets 1.100" run_script apply
check "apply: a patch release reports applied=true" out_has applied true

new_sandbox apply-nix-garbage
export MSRV_TARGET_MINOR=100
export FAKE_NIX_RUSTC="not-a-version"
check_not "apply: an unreadable nix rustc version fails the run" run_script apply

new_sandbox apply-drift
export MSRV_TARGET_MINOR=100
printf 'no version here\n' > flake.nix
check_not "apply: a site that no longer matches fails the run" run_script apply
check "apply: names the drifted file" grep -qF 'flake.nix' "$sb/stderr"

new_sandbox apply-bad-target
for bad_target in '100; touch x' '' '1.100' '-1' '$(id)' '99999999'; do
  if MSRV_TARGET_MINOR="$bad_target" bash "$script" apply > "$sb/stdout" 2> "$sb/stderr"; then
    bad "apply: rejects target '$bad_target'"
  else
    ok "apply: rejects target '$bad_target'"
  fi
done
check "apply: a rejected target leaves the tree clean" git diff --quiet
check_not "apply: a rejected target runs nothing" test -e x

new_sandbox apply-not-newer
export MSRV_TARGET_MINOR=99
check_not "apply: a target equal to rust-version fails the run" run_script apply

# CHANGELOG variants.
entry_text='- The minimum supported Rust version is now 1.100, the newest stable release. Older Rust releases no
  longer receive upstream fixes. Homebrew installs are unaffected; `cargo install` and source builds
  need Rust 1.100 or newer. The Nix dev shell takes its `rustc` from an updated `nixpkgs` input.'

# changelog_case <name> <input> <expected>
changelog_case() {
  new_sandbox "changelog-$1"
  export MSRV_TARGET_MINOR=100
  printf '%s' "$2" > CHANGELOG.md
  printf '%s' "$3" > "$sb/want"
  run_script apply
  if diff -u "$sb/want" CHANGELOG.md > "$sb/diff"; then ok "changelog: $1"; else bad "changelog: $1"; cat "$sb/diff"; fi
}

changelog_case "empty Unreleased gets a Changed section" '# Changelog

## [Unreleased]

## [0.29.0] - 2026-10-25

### Added
- old
' "# Changelog

## [Unreleased]

### Changed
$entry_text

## [0.29.0] - 2026-10-25

### Added
- old
"

changelog_case "Unreleased with only Added gets Changed above it" '# Changelog

## [Unreleased]

### Added
- new thing

## [0.29.0] - 2026-10-25
' "# Changelog

## [Unreleased]

### Changed
$entry_text

### Added
- new thing

## [0.29.0] - 2026-10-25
"

changelog_case "existing Changed section gets the entry appended" '# Changelog

## [Unreleased]

### Changed
- earlier change
  continued line

### Fixed
- a fix

## [0.29.0] - 2026-10-25

### Changed
- history
' "# Changelog

## [Unreleased]

### Changed
- earlier change
  continued line
$entry_text

### Fixed
- a fix

## [0.29.0] - 2026-10-25

### Changed
- history
"

changelog_case "Changed as the last section of Unreleased at end of file" '# Changelog

## [Unreleased]

### Changed
- earlier change
' "# Changelog

## [Unreleased]

### Changed
- earlier change
$entry_text
"

# A second apply over an already-edited CHANGELOG must not add the entry twice.
new_sandbox changelog-idempotent
export MSRV_TARGET_MINOR=100
run_script apply
edited_changelog="$(cat CHANGELOG.md)"
write_tree
printf '%s\n' "$edited_changelog" > CHANGELOG.md
run_script apply
check "changelog: a second run does not duplicate the entry" test "$(grep -c 'minimum supported Rust version is now 1.100' CHANGELOG.md)" = 1

# The repository's own files: every place that names the MSRV is covered.
new_sandbox apply-real-tree
cur="$(sed -n 's/^rust-version = "1\.\([0-9][0-9]*\).*/\1/p' "$root/Cargo.toml" | head -n1)"
nxt=$((cur + 1))
for f in Cargo.toml .github/workflows/rust.yml CONTRIBUTING.md docs/INSTALL.md flake.nix flake.lock CHANGELOG.md; do
  mkdir -p "$(dirname "$f")"
  cp "$root/$f" "$f"
done
git add -A
git commit -q -m "real files"
export MSRV_TARGET_MINOR="$nxt"
export FAKE_NIX_RUSTC="1.$nxt.0"
check "real tree: apply succeeds on the repository's own files" run_script apply
check "real tree: Cargo.toml raised" grep -qxF "rust-version = \"1.$nxt\"" Cargo.toml
# Spellings of the old version that must be gone (CHANGELOG history may keep them).
check_not "real tree: no stale MSRV spelling remains outside CHANGELOG" grep -n \
  -e "rust-version = \"1.$cur\"" -e "toolchain: \"1.$cur\"" -e "\"stable\", \"1.$cur\"" \
  -e "≥1.$cur" -e "MSRV is 1.$cur" -e "MSRV (1.$cur)" \
  Cargo.toml .github/workflows/rust.yml CONTRIBUTING.md docs/INSTALL.md flake.nix

# ===========================================================================
# publish
# ===========================================================================
bot_email="41898282+github-actions[bot]@users.noreply.github.com"

# ready_to_publish <name>: a sandbox where apply already ran.
ready_to_publish() {
  new_sandbox "$1"
  export MSRV_TARGET_MINOR=100
  export GH_TOKEN="tok-SECRET-value"
  export GITHUB_REPOSITORY="example/repo"
  run_script apply
}
remote_ref() { git -C "$sb/origin.git" rev-parse --verify --quiet "refs/heads/chore/msrv-100"; }
remote_has_branch() { remote_ref > /dev/null; }
one_pr_open() {
  echo '[{"number":7,"state":"OPEN"}]' > "$FAKE_GH_HEAD"
  echo '[{"number":7,"headRefName":"chore/msrv-100"}]' > "$FAKE_GH_OPEN"
}
gh_call() { grep -F -- "$1" "$FAKE_GH_LOG"; }
pr_create_has() { gh_call 'pr create' | grep -qF -- "$1"; }
pr_edit_has() { gh_call 'pr edit 7' | grep -qF -- "$1"; }

ready_to_publish publish-new
check "publish: new branch and PR succeeds" run_script publish
check "publish: pushes chore/msrv-100" remote_has_branch
check "publish: the pushed commit holds the edit set" test "$(git -C "$sb/origin.git" show chore/msrv-100:Cargo.toml | grep -cx 'rust-version = "1.100"')" = 1
check "publish: the commit is the bot's" test "$(git -C "$sb/origin.git" log -1 --format=%ae chore/msrv-100)" = "$bot_email"
check "publish: one commit on top of main" test "$(git -C "$sb/origin.git" rev-list --count main..chore/msrv-100)" = 1
check "publish: commit subject" test "$(git -C "$sb/origin.git" log -1 --format=%s chore/msrv-100)" = "chore: raise the minimum supported Rust version to 1.100"
check "publish: PR is created against main" pr_create_has '--base main'
check "publish: PR head is the tracker branch" pr_create_has '--head chore/msrv-100'
check "publish: PR carries the minor label" pr_create_has '--label minor'
check "publish: PR targets the given repository" pr_create_has '--repo example/repo'
check "publish: PR title matches the squash commit subject" pr_create_has '--title chore: raise the minimum supported Rust version to 1.100'
check "publish: PR body says nothing merges automatically" grep -qi 'not merge automatically' "$FAKE_GH_BODY"
check "publish: PR body keeps checkboxes for the merger" grep -qF -- '- [ ]' "$FAKE_GH_BODY"
check "publish: reports result=created" out_has result created
check_not "publish: the token never reaches the output" grep -q 'tok-SECRET-value' "$sb/stdout" "$sb/stderr" "$FAKE_GH_LOG"

# Idempotent: the same run again changes nothing and opens nothing new.
one_pr_open
git checkout -q main
before="$(remote_ref)"
: > "$FAKE_GH_LOG"
run_script apply
check "publish: rerun over an identical branch succeeds" run_script publish
check "publish: rerun does not move the branch" test "$(remote_ref)" = "$before"
check_not "publish: rerun does not open a second PR" grep -q 'pr create' "$FAKE_GH_LOG"
check "publish: rerun reports result=unchanged" out_has result unchanged

# Open PR whose branch holds only the tracker's commit: refreshed.
ready_to_publish publish-refresh
run_script publish
one_pr_open
git checkout -q main
printf 'extra\n' > NEWFILE.md
git add NEWFILE.md
git commit -q -m "main moves"
git push -q origin main 2> /dev/null
old_tip="$(remote_ref)"
: > "$FAKE_GH_LOG"
git checkout -q -- .
run_script apply
check "publish: refresh of a tracker-only branch succeeds" run_script publish
check "publish: refresh moves the branch onto the new main" test "$(git -C "$sb/origin.git" merge-base main chore/msrv-100)" = "$(git -C "$sb/origin.git" rev-parse main)"
check "publish: refresh really changed the branch" test "$(remote_ref)" != "$old_tip"
check "publish: refresh edits the existing PR" grep -q 'pr edit 7' "$FAKE_GH_LOG"
check_not "publish: refresh does not create a PR" grep -q 'pr create' "$FAKE_GH_LOG"
check "publish: refresh re-applies the minor label" pr_edit_has '--add-label minor'
check "publish: refresh reports result=updated" out_has result updated

# A person added a commit (for example a lint fix): never overwritten.
ready_to_publish publish-human
run_script publish
git checkout -q main
git clone -q "$sb/origin.git" "$sb/human" 2> /dev/null
(
  cd "$sb/human"
  git checkout -q chore/msrv-100
  printf 'fix\n' > lintfix.rs
  git add lintfix.rs
  git -c user.name=Human -c user.email=human@example.invalid commit -q -m "fix new lint"
  git push -q origin chore/msrv-100 2> /dev/null
)
one_pr_open
human_tip="$(remote_ref)"
: > "$FAKE_GH_LOG"
run_script apply
check "publish: a branch with a person's commit exits 0" run_script publish
check "publish: the person's commit is never overwritten" test "$(remote_ref)" = "$human_tip"
check_not "publish: the PR is left alone" grep -q -e 'pr create' -e 'pr edit' "$FAKE_GH_LOG"
check "publish: reports result=skipped-edited" out_has result skipped-edited

# A PR for the branch was closed without merging: someone chose to skip it.
ready_to_publish publish-closed
echo '[{"number":7,"state":"CLOSED"}]' > "$FAKE_GH_HEAD"
check "publish: a closed PR for the branch exits 0" run_script publish
check_not "publish: a closed PR means no push" remote_has_branch
check_not "publish: a closed PR means no new PR" grep -q 'pr create' "$FAKE_GH_LOG"
check "publish: reports result=skipped-closed" out_has result skipped-closed

# Another minor's tracker PR is still open: one PR at a time.
ready_to_publish publish-other
echo '[{"number":5,"headRefName":"chore/msrv-99"},{"number":6,"headRefName":"feature/other"}]' > "$FAKE_GH_OPEN"
check "publish: another open tracker PR exits 0" run_script publish
check_not "publish: no second tracker PR is created" grep -q 'pr create' "$FAKE_GH_LOG"
check_not "publish: no second tracker branch is pushed" remote_has_branch
check "publish: reports result=skipped-other-pr" out_has result skipped-other-pr

ready_to_publish publish-unrelated
echo '[{"number":6,"headRefName":"feature/other"}]' > "$FAKE_GH_OPEN"
check "publish: an unrelated open PR does not block" run_script publish
check "publish: an unrelated open PR still gets the tracker PR" out_has result created

# Input validation.
ready_to_publish publish-bad-input
for bad_target in '100; touch x' '' '$(id)'; do
  if MSRV_TARGET_MINOR="$bad_target" bash "$script" publish > "$sb/stdout" 2> "$sb/stderr"; then
    bad "publish: rejects target '$bad_target'"
  else
    ok "publish: rejects target '$bad_target'"
  fi
done
check_not "publish: rejected targets create no branch" remote_has_branch

ready_to_publish publish-no-token
check_not "publish: refuses to run without a token" env -u GH_TOKEN bash "$script" publish

ready_to_publish publish-bad-repo
check_not "publish: rejects a malformed GITHUB_REPOSITORY" env GITHUB_REPOSITORY='a b;c' bash "$script" publish

# Nothing to commit (apply never ran): fail instead of opening an empty PR.
new_sandbox publish-empty
export MSRV_TARGET_MINOR=100 GH_TOKEN="tok-SECRET-value" GITHUB_REPOSITORY="example/repo"
check_not "publish: refuses to open a PR with no changes" run_script publish
check_not "publish: no PR for an empty change" grep -q 'pr create' "$FAKE_GH_LOG"

# ---- usage -------------------------------------------------------------------
new_sandbox usage
check_not "usage: no subcommand fails" run_script
check_not "usage: unknown subcommand fails" run_script frobnicate

printf '\nmsrv-track tests: %d passed, %d failed\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]

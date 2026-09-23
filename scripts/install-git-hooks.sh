#!/usr/bin/env bash
# install-git-hooks.sh — chain scripts/clean-target.sh into .git/hooks/post-merge.
#
# Why: `.git/hooks/` is not version-controlled, so the repo ships this
# installer rather than a checked-in hook. `core.hooksPath` is deliberately
# not used — switching to it bypasses `.git/hooks/` entirely and would
# silently disable Git LFS unless its shims were reproduced by hand.
#
# Idempotent: re-running skips if our marker is already present, and refuses
# to overwrite a post-merge hook it doesn't recognize (neither the git-lfs
# shim nor its own prior install) rather than clobbering someone's own hook.
#
# Usage: install-git-hooks.sh [path-to-repo-root]
set -Eeuo pipefail

root="${1:-.}"
hook_dir="${root}/.git/hooks"
hook="${hook_dir}/post-merge"
marker="# install-git-hooks.sh: clean-target chain"

if [[ ! -d "$hook_dir" ]]; then
  printf 'install-git-hooks: %s not found — not a git repo root?\n' "$hook_dir" >&2
  exit 1
fi

if [[ -f "$hook" ]] && grep -qF "$marker" "$hook"; then
  printf 'install-git-hooks: post-merge hook already installed — nothing to do\n'
  exit 0
fi

if [[ -f "$hook" ]] && ! grep -q 'git lfs post-merge' "$hook"; then
  printf 'install-git-hooks: %s exists and is not the git-lfs shim or our own install — refusing to overwrite\n' "$hook" >&2
  exit 1
fi

cat > "$hook" <<EOF
#!/bin/sh
$marker
command -v git-lfs >/dev/null 2>&1 || { printf >&2 "\n%s\n\n" "This repository is configured for Git LFS but 'git-lfs' was not found on your path. If you no longer wish to use Git LFS, remove this hook by deleting the 'post-merge' file in the hooks directory (set by 'core.hookspath'; usually '.git/hooks')."; exit 2; }
git lfs post-merge "\$@"
lfs_status=\$?

# Advisory only: target/ pruning must never fail the merge.
"\$(git rev-parse --show-toplevel)/scripts/clean-target.sh" 2>&1 || true

exit "\$lfs_status"
EOF
chmod +x "$hook"

printf 'install-git-hooks: installed chained post-merge hook at %s\n' "$hook"

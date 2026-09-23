#!/usr/bin/env bash
# clean-target.sh — age-based prune of stale `target/` build artifacts.
#
# Why: Cargo never garbage-collects `target/` — stale fingerprints accumulate
# permanently, and nothing else in this repo prunes them (`cargo-sweep` is
# deprecated upstream, Cargo's own `-Zgc` replacement is nightly-only). This
# is deliberately age-based rather than `cargo clean`: a full clean forces a
# llama-cpp-sys-2 CMake rebuild (see .cargo/config.toml:1-5), so pruning only
# artifacts untouched for N days keeps warm build state intact.
#
# Usage: clean-target.sh [--dry-run] [--days N] [path-to-repo-root]
set -Eeuo pipefail

dry_run=0
days=14
root="."

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      dry_run=1
      shift
      ;;
    --days)
      days="${2:?--days requires a value}"
      shift 2
      ;;
    *)
      root="$1"
      shift
      ;;
  esac
done

target_dir="${root}/target"

if [[ ! -d "$target_dir" ]]; then
  printf 'clean-target: no %s — nothing to prune\n' "$target_dir"
  exit 0
fi

# target/debug/incremental/ is pure rebuild state (never a build input) and
# the single largest contributor, so prune it first and most aggressively.
prune_dirs=("${target_dir}/debug/incremental" "$target_dir")

total_bytes=0
candidates=()

while IFS= read -r -d '' entry; do
  candidates+=("$entry")
  size="$(du -sk "$entry" 2>/dev/null | cut -f1)"
  total_bytes=$(( total_bytes + size * 1024 ))
done < <(find "${prune_dirs[@]}" -mindepth 1 -maxdepth 1 -mtime "+${days}" -print0 2>/dev/null | sort -zu)

if [[ ${#candidates[@]} -eq 0 ]]; then
  printf 'clean-target: nothing older than %s day(s) under %s\n' "$days" "$target_dir"
  exit 0
fi

human_total="$(( total_bytes / 1024 / 1024 )) MB"

if [[ "$dry_run" -eq 1 ]]; then
  printf 'clean-target: --dry-run — would remove %d entr(y/ies), ~%s:\n' "${#candidates[@]}" "$human_total"
  printf '  %s\n' "${candidates[@]}"
  exit 0
fi

for entry in "${candidates[@]}"; do
  rm -rf -- "$entry"
done

printf 'clean-target: removed %d entr(y/ies), ~%s freed from %s\n' "${#candidates[@]}" "$human_total" "$target_dir"

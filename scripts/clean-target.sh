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
# Usage: clean-target.sh [--dry-run] [--days N] [--grace-hours N]
#                         [--no-hash-sweep] [path-to-repo-root]
set -Eeuo pipefail

dry_run=0
days=14
grace_hours=24
hash_sweep=1
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
    --grace-hours)
      grace_hours="${2:?--grace-hours requires a value}"
      shift 2
      ;;
    --no-hash-sweep)
      hash_sweep=0
      shift
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
else
  human_total="$(( total_bytes / 1024 / 1024 )) MB"

  if [[ "$dry_run" -eq 1 ]]; then
    printf 'clean-target: --dry-run — would remove %d entr(y/ies), ~%s:\n' "${#candidates[@]}" "$human_total"
    printf '  %s\n' "${candidates[@]}"
  else
    for entry in "${candidates[@]}"; do
      rm -rf -- "$entry"
    done
    printf 'clean-target: removed %d entr(y/ies), ~%s freed from %s\n' "${#candidates[@]}" "$human_total" "$target_dir"
  fi
fi

# --- Hash-group sweep -------------------------------------------------------
#
# Why: every distinct feature-flag/profile combination Cargo builds against
# the same crate leaves its own `<crate>-<16-hex-hash>` fingerprint dir (and
# matching `target/debug/deps/*<hash>*` output files) behind — Cargo never
# reuses or reclaims a superseded hash once a newer one exists for the same
# crate. These accumulate well inside the `--days` window above (a single
# review session can produce 20+ hash variants of one crate — see the rmcp
# CVE remediation session, 2026-09-24), so the age-based sweep alone never
# touches them. This sweep is conservative by design: it only removes a
# hash-group member once a *newer sibling* for the same crate has appeared
# (proof the old one is genuinely superseded, not just idle), and only once
# that member has been untouched for `--grace-hours` (default 24) — so a
# build in progress is never swept out from under it.
#
# If every sibling in a group is stale (no fresh one has appeared), this
# sweep leaves the group alone entirely — that's an idle crate, not a
# superseded one, and is the age-based sweep's job above, not this one's.

is_stale_group() {
  # Returns success (0/stale) if no file under $1 was modified within the
  # last $2 hours. -mmin is portable across GNU and BSD find.
  local dir="$1" grace_hours="$2"
  local grace_min=$(( grace_hours * 60 ))
  [[ -z "$(find "$dir" -type f -mmin "-${grace_min}" -print -quit 2>/dev/null)" ]]
}

hash_sweep_report=()
hash_sweep_bytes=0

if [[ "$hash_sweep" -eq 1 ]]; then
  fp_dir="${target_dir}/debug/.fingerprint"

  if [[ -d "$fp_dir" ]]; then
    fp_lines="$(
      find "$fp_dir" -mindepth 1 -maxdepth 1 -type d -print0 2>/dev/null \
        | while IFS= read -r -d '' d; do
            name="$(basename "$d")"
            if [[ "$name" =~ ^(.+)-([0-9a-f]{16})$ ]]; then
              printf '%s\t%s\t%s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "$d"
            fi
          done | sort
    )"

    group_base="" group_dirs=() group_hashes=()

    flush_group() {
      local n="${#group_dirs[@]}"
      [[ "$n" -le 1 ]] && return 0

      local any_fresh=0 i
      for (( i = 0; i < n; i++ )); do
        if ! is_stale_group "${group_dirs[$i]}" "$grace_hours"; then
          any_fresh=1
          break
        fi
      done
      [[ "$any_fresh" -eq 1 ]] || return 0

      for (( i = 0; i < n; i++ )); do
        if is_stale_group "${group_dirs[$i]}" "$grace_hours"; then
          hash_sweep_report+=("${group_dirs[$i]}")
          local h="${group_hashes[$i]}"
          while IFS= read -r -d '' f; do
            hash_sweep_report+=("$f")
          done < <(find "${target_dir}/debug/deps" "${target_dir}/debug/build" \
                     -mindepth 1 -maxdepth 1 -name "*${h}*" -print0 2>/dev/null)
        fi
      done
    }

    if [[ -n "$fp_lines" ]]; then
      while IFS=$'\t' read -r base hash dir; do
        if [[ "$base" != "$group_base" && -n "$group_base" ]]; then
          flush_group
          group_dirs=()
          group_hashes=()
        fi
        group_dirs+=("$dir")
        group_hashes+=("$hash")
        group_base="$base"
      done <<< "$fp_lines"
      flush_group
    fi
  fi
fi

if [[ ${#hash_sweep_report[@]} -eq 0 ]]; then
  printf 'clean-target: hash-sweep — no superseded hash-group entries older than %sh\n' "$grace_hours"
else
  for entry in "${hash_sweep_report[@]}"; do
    size="$(du -sk "$entry" 2>/dev/null | cut -f1)"
    hash_sweep_bytes=$(( hash_sweep_bytes + size * 1024 ))
  done
  human_hash_total="$(( hash_sweep_bytes / 1024 / 1024 )) MB"

  if [[ "$dry_run" -eq 1 ]]; then
    printf 'clean-target: --dry-run — hash-sweep would remove %d entr(y/ies), ~%s:\n' \
      "${#hash_sweep_report[@]}" "$human_hash_total"
    printf '  %s\n' "${hash_sweep_report[@]}"
  else
    for entry in "${hash_sweep_report[@]}"; do
      rm -rf -- "$entry"
    done
    printf 'clean-target: hash-sweep removed %d entr(y/ies), ~%s freed (superseded, >%sh idle)\n' \
      "${#hash_sweep_report[@]}" "$human_hash_total" "$grace_hours"
  fi
fi

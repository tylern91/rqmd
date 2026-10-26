#!/usr/bin/env bash
# Generate THIRD-PARTY-LICENSES.md for the rqmd binary with cargo-about
# (about.toml, packaging/licenses/about.hbs), then check that every crate
# compiled into the binary is listed in it.
#
# Usage: scripts/gen-third-party-licenses.sh [output-file]
# Needs cargo-about on PATH (CI installs the pinned version).
set -Eeuo pipefail

out="${1:-THIRD-PARTY-LICENSES.md}"
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Download every crate source first; the steps below then run offline.
cargo fetch --locked --quiet

# --frozen (locked and offline) also skips the clearlydefined.io lookup, so the
# texts come from the crate sources only and the output is reproducible.
cargo about generate --frozen --fail \
  -m crates/rqmd-cli/Cargo.toml \
  -o "$out" packaging/licenses/about.hbs

# Crates compiled into rqmd-cli on each release target: normal dependencies
# only, without proc-macros (they run at build time) and without rqmd's own
# crates.
missing=0
for target in aarch64-apple-darwin x86_64-unknown-linux-gnu; do
  while IFS= read -r crate; do
    if ! grep -qxF -- "- $crate" "$out"; then
      echo "::error::$crate ($target) is not listed in $out" >&2
      missing=1
    fi
  done < <(cargo tree --frozen --color never -p rqmd-cli -e normal,no-proc-macro \
    --target "$target" --prefix none --format '{p}' \
    | sed -E 's/ \(\*\)$//; s/ \(.*\)$//; s/ v/ /' | grep -v '^rqmd' | sort -u)
done

if [ "$missing" -ne 0 ]; then
  exit 1
fi
echo "$out lists every crate compiled into rqmd"

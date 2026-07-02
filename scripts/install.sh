#!/usr/bin/env bash
# install.sh — content-aware install for rqmd
#
# cargo install --path skips reinstalling when the crate version hasn't changed.
# This script bypasses that version check: cargo build fingerprints source files,
# so a no-op build means no rebuild, and we copy whatever binary just came out
# into the cargo bin dir. No --force, no manual version bump required.
#
# Usage:
#   ./scripts/install.sh                        # dist profile (default)
#   RQMD_PROFILE=release ./scripts/install.sh   # release profile
#   ./scripts/install.sh --features ort-backend # pass extra cargo flags through
set -euo pipefail

PROFILE="${RQMD_PROFILE:-dist}"
BIN_DIR="${CARGO_HOME:-$HOME/.cargo}/bin"

# Build (cargo's fingerprint makes this a no-op when nothing changed)
cargo build --profile "$PROFILE" -p rqmd-cli "$@"

# Locate the output; 'dev' profile writes to target/debug, everything else to target/<profile>
if [[ "$PROFILE" == "dev" ]]; then
  OUT_DIR="target/debug"
else
  OUT_DIR="target/$PROFILE"
fi

# Atomic copy: install(1) writes to a temp file then renames, safe to replace a running binary
install -m 0755 "$OUT_DIR/rqmd" "$BIN_DIR/rqmd"

echo "installed rqmd → $BIN_DIR/rqmd ($("$BIN_DIR/rqmd" --version 2>/dev/null || echo 'version unknown'))"

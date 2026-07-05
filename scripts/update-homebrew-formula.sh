#!/usr/bin/env bash
# update-homebrew-formula.sh <version>
#
# Fetches the release SHA-256 checksums from GitHub and updates the Homebrew
# formula template at packaging/homebrew/rqmd.rb.
#
# When HOMEBREW_TAP_TOKEN is set, also clones github.com/tylern91/homebrew-rqmd
# and pushes the updated Formula/rqmd.rb.
#
# Usage:
#   ./scripts/update-homebrew-formula.sh v0.3.0      # from the repo root
#   VERSION=v0.3.0 ./scripts/update-homebrew-formula.sh

set -euo pipefail

VERSION="${1:-${VERSION:-}}"
if [[ -z "$VERSION" ]]; then
  echo "Usage: $0 <version>  (e.g. v0.3.0)" >&2
  exit 1
fi

# Normalize: always have the 'v' prefix for URLs / git tags,
# and the bare version (without 'v') for the formula version field.
VERSION="${VERSION#v}"          # strip leading v if present
TAG="v${VERSION}"               # canonical tag: v0.3.0
BARE="${VERSION}"               # bare: 0.3.0

REPO="tylern91/rqmd"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TEMPLATE="$(cd "$(dirname "$0")/.." && pwd)/packaging/homebrew/rqmd.rb"

if [[ ! -f "$TEMPLATE" ]]; then
  echo "Template not found: $TEMPLATE" >&2
  exit 1
fi

fetch_sha256() {
  local name_suffix="$1"
  local tarball="rqmd-${TAG}-${name_suffix}.tar.gz"
  local sha_file="${tarball}.sha256"
  local url="${BASE_URL}/${sha_file}"
  echo "Fetching ${sha_file} …" >&2
  # The .sha256 file contains "<hash>  <filename>" — extract just the hash.
  curl -fsSL "$url" | awk '{print $1}'
}

SHA256_MACOS_ARM64="$(fetch_sha256 aarch64-apple-darwin)"
SHA256_LINUX_X86="$(fetch_sha256 x86_64-unknown-linux-gnu)"

echo "  macOS arm64 : ${SHA256_MACOS_ARM64}" >&2
echo "  Linux x86_64: ${SHA256_LINUX_X86}" >&2

# Substitute placeholders in the template.
UPDATED="$(sed \
  -e "s|RQMD_VERSION|${TAG}|g" \
  -e "s|RQMD_BARE_VERSION|${BARE}|g" \
  -e "s|RQMD_SHA256_MACOS_ARM64|${SHA256_MACOS_ARM64}|g" \
  -e "s|RQMD_SHA256_LINUX_X86|${SHA256_LINUX_X86}|g" \
  "$TEMPLATE")"

# Always write the updated formula back to the template path.
printf '%s\n' "$UPDATED" > "$TEMPLATE"
echo "Updated ${TEMPLATE}" >&2

# Optionally push to the Homebrew tap repo.
if [[ -n "${HOMEBREW_TAP_TOKEN:-}" ]]; then
  TAP_REPO="tylern91/homebrew-rqmd"
  TAP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TAP_DIR"' EXIT

  echo "Cloning tap repo ${TAP_REPO} …" >&2
  git clone --depth 1 \
    "https://x-access-token:${HOMEBREW_TAP_TOKEN}@github.com/${TAP_REPO}.git" \
    "$TAP_DIR"

  mkdir -p "${TAP_DIR}/Formula"
  cp "$TEMPLATE" "${TAP_DIR}/Formula/rqmd.rb"

  git -C "$TAP_DIR" config user.name  "github-actions[bot]"
  git -C "$TAP_DIR" config user.email "tylern91@users.noreply.github.com"

  git -C "$TAP_DIR" add Formula/rqmd.rb
  if git -C "$TAP_DIR" diff --cached --quiet; then
    echo "No formula changes to push." >&2
  else
    git -C "$TAP_DIR" commit -m "chore: update rqmd formula to ${TAG}"
    git -C "$TAP_DIR" push
    echo "Pushed Formula/rqmd.rb to ${TAP_REPO}" >&2
  fi
fi

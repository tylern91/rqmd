#!/usr/bin/env bash
# Reads changed file paths on stdin and prints "true" when any of them can
# change what `rust.yml` builds or tests, else "false". The path list is the
# former `paths:` filter of that workflow plus the acceptance scripts the
# ac-gate job runs.
set -Eeuo pipefail

while IFS= read -r file; do
  case "$file" in
    crates/* | .cargo/* | \
      Cargo.toml | Cargo.lock | CHANGELOG.md | \
      scripts/check-version-sync.sh | \
      scripts/run-acceptance-tests.sh | \
      scripts/lint-acceptance-tests.sh | \
      scripts/ci-rust-changed.sh | \
      .github/workflows/rust.yml)
      echo true
      exit 0
      ;;
  esac
done
echo false

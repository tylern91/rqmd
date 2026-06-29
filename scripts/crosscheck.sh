#!/usr/bin/env bash
# crosscheck.sh — Diff Rust qmd vs TypeScript qmd on a shared fixture corpus.
#
# Runs the same query set through both binaries (--format json), then compares
# the returned docid sets (top-K overlap). Ordering may differ due to RRF float
# math; set overlap is the quality signal.
#
# Prerequisites:
#   - TypeScript qmd: `qmd` on PATH (run `bun link` in the repo root)
#   - Rust qmd: `cargo build --profile dist -p qmd-cli` already built
#   - Models downloaded (i.e. `qmd embed` run at least once)
#
# Usage:
#   ./scripts/crosscheck.sh [TOP_K] [MIN_OVERLAP_PCT]
#
# Arguments:
#   TOP_K              Number of results to compare (default: 5)
#   MIN_OVERLAP_PCT    Minimum acceptable set overlap %, 0-100 (default: 60)
#
# Exit code: 0 if all queries meet the overlap threshold, 1 otherwise.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUST_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS_DIR="${RUST_ROOT}/qmd-cli/eval-docs"
RUST_BIN="${RUST_ROOT}/../target/dist/qmd"
TS_BIN="qmd"

TOP_K="${1:-5}"
MIN_OVERLAP="${2:-60}"

# ── Sanity checks ──────────────────────────────────────────────────────────────

if ! command -v "${TS_BIN}" &>/dev/null; then
  echo "ERROR: TypeScript qmd not found on PATH. Run 'bun link' in ${REPO_ROOT}." >&2
  exit 1
fi

if [[ ! -x "${RUST_BIN}" ]]; then
  echo "ERROR: Rust binary not found at ${RUST_BIN}." >&2
  echo "       Run: cargo build --profile dist -p qmd-cli" >&2
  exit 1
fi

if [[ ! -d "${CORPUS_DIR}" ]]; then
  echo "ERROR: Corpus not found at ${CORPUS_DIR}." >&2
  exit 1
fi

echo "=== crosscheck.sh ==="
echo "  Corpus:        ${CORPUS_DIR}"
echo "  TS  binary:    $(command -v ${TS_BIN})"
echo "  Rust binary:   ${RUST_BIN}"
echo "  TOP_K:         ${TOP_K}"
echo "  Min overlap:   ${MIN_OVERLAP}%"
echo ""

# ── Index setup ────────────────────────────────────────────────────────────────

TS_IDX="$(mktemp -d /tmp/qmd-crosscheck-ts-XXXXXX)"
RS_IDX="$(mktemp -d /tmp/qmd-crosscheck-rs-XXXXXX)"
trap 'rm -rf "${TS_IDX}" "${RS_IDX}"' EXIT

echo "Indexing corpus with TS qmd → ${TS_IDX}"
QMD_INDEX_DIR="${TS_IDX}" "${TS_BIN}" collection add "${CORPUS_DIR}" --name crosscheck >/dev/null
QMD_INDEX_DIR="${TS_IDX}" "${TS_BIN}" embed >/dev/null

echo "Indexing corpus with Rust qmd → ${RS_IDX}"
QMD_INDEX_DIR="${RS_IDX}" "${RUST_BIN}" collection add "${CORPUS_DIR}" --name crosscheck >/dev/null
QMD_INDEX_DIR="${RS_IDX}" "${RUST_BIN}" embed >/dev/null

echo ""

# ── Query set ─────────────────────────────────────────────────────────────────

QUERIES=(
  "what is reciprocal rank fusion"
  "machine learning gradient descent"
  "distributed systems CAP theorem"
  "remote work productivity tips"
  "startup fundraising series A"
  "API design REST principles"
  "product launch retrospective"
  "neural network embeddings"
)

# ── Comparison ────────────────────────────────────────────────────────────────

pass=0
fail=0

for query in "${QUERIES[@]}"; do
  # Extract top-K docids from JSON output (field: "docid" or "id")
  ts_docids=$(
    QMD_INDEX_DIR="${TS_IDX}" "${TS_BIN}" query "${query}" -n "${TOP_K}" --format json 2>/dev/null \
      | python3 -c "import sys,json; r=json.load(sys.stdin); print('\n'.join(d.get('docid','') or d.get('id','') for d in r))" \
      2>/dev/null | grep -v '^$' | sort
  )
  rs_docids=$(
    QMD_INDEX_DIR="${RS_IDX}" "${RUST_BIN}" query "${query}" -n "${TOP_K}" --format json 2>/dev/null \
      | python3 -c "import sys,json; r=json.load(sys.stdin); print('\n'.join(d.get('docid','') or d.get('id','') for d in r))" \
      2>/dev/null | grep -v '^$' | sort
  )

  if [[ -z "${ts_docids}" && -z "${rs_docids}" ]]; then
    echo "[SKIP] \"${query}\" — both returned empty (models not loaded?)"
    continue
  fi

  # Compute intersection size
  intersection=$(comm -12 <(echo "${ts_docids}") <(echo "${rs_docids}") | wc -l | tr -d ' ')
  union=$(comm -23 <(echo "${ts_docids}") <(echo "${rs_docids}") | wc -l | tr -d ' ')
  union=$(( intersection + union + $(comm -13 <(echo "${ts_docids}") <(echo "${rs_docids}") | wc -l | tr -d ' ') ))
  ts_count=$(echo "${ts_docids}" | grep -c . || true)

  if [[ "${ts_count}" -eq 0 ]]; then
    overlap_pct=0
  else
    overlap_pct=$(( intersection * 100 / ts_count ))
  fi

  if [[ "${overlap_pct}" -ge "${MIN_OVERLAP}" ]]; then
    echo "[PASS ${overlap_pct}%] \"${query}\""
    (( pass++ )) || true
  else
    echo "[FAIL ${overlap_pct}%] \"${query}\" — intersection=${intersection}/${ts_count}"
    echo "  TS  docids: $(echo "${ts_docids}" | tr '\n' ' ')"
    echo "  Rust docids: $(echo "${rs_docids}" | tr '\n' ' ')"
    (( fail++ )) || true
  fi
done

echo ""
echo "=== Results: ${pass} PASS / ${fail} FAIL (threshold: ${MIN_OVERLAP}% overlap) ==="

if [[ "${fail}" -gt 0 ]]; then
  exit 1
fi

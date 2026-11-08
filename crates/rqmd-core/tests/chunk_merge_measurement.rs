//! Measurement for #126: how many chunks fall below a candidate minimum size, and what would a
//! "merge a tiny chunk into its neighbour" rule change?
//!
//! The splitter cuts a document into windows of `CHUNK_SIZE_CHARS` with `CHUNK_OVERLAP_CHARS` of
//! overlap, so the last chunk of a document can be a short tail and a short document is a single
//! short chunk. This reads the active documents of an existing index, runs the real public
//! splitter on each distinct document and simulates the merge for the candidate minimums 200, 400
//! and 800 characters. It only measures: nothing here changes chunking, the embed fingerprint or
//! the index, and the index is opened read-only.
//!
//! One `#[ignore]` test, run by hand against an index (a copy is safest):
//!
//! ```text
//! RQMD_MEASURE_INDEX=/path/to/index.sqlite \
//!   cargo test -p rqmd-core --test chunk_merge_measurement -- --ignored --nocapture
//! ```
//!
//! Add `--features ast-chunking` to measure source files with the declaration-boundary chunker the
//! `rqmd` binary uses when it is built with that feature. A document that appears in several
//! collections or paths with identical text is chunked once, by its content hash, and counted once
//! in the total and once per collection that holds it.
//!
//! The merge rule: a chunk below the minimum is merged into the previous chunk, or into the next
//! one when it is the first. A merged chunk is the union of the two overlapping windows, so the
//! overlap is counted once; the report also gives the no-overlap-credit sum as an upper bound. A
//! document with a single chunk has no neighbour and is reported separately. Token counts are an
//! estimate (characters / 3.5), not a tokenizer measurement.
//!
//! The merge simulation and the report are plain code with ordinary tests on synthetic strings,
//! so CI covers them without a model or an index of real documents.

use anyhow::{Context, Result};
use rqmd_core::chunking::{CHUNK_OVERLAP_CHARS, CHUNK_SIZE_CHARS, CHUNK_WINDOW_CHARS};
use rqmd_core::chunking::{ast_chunking_compiled, chunk_document_for_path};
use rqmd_core::types::Chunk;
use rusqlite::{Connection, OpenFlags};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Path of the `index.sqlite` to measure (required, no default).
const ENV_INDEX: &str = "RQMD_MEASURE_INDEX";

/// Candidate minimum chunk sizes, in characters.
const MINIMUMS: [usize; 3] = [200, 400, 800];

/// Embedding input limit in tokens (`safe_limit` in `LlamaCppBackend::embed_texts`).
const TOKEN_LIMIT: f64 = 2044.0;
/// Rough characters per token used for the estimate. Mixed prose and code, not a tokenizer.
const CHARS_PER_TOKEN: f64 = 3.5;

// ── Pure logic (covered by the non-ignored tests) ────────────────────────────

/// A chunk reduced to what the merge simulation needs. `pos` and `bytes` are byte offsets, as the
/// splitter uses them; `chars` is the length the minimums and the token estimate are judged on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    pos: usize,
    bytes: usize,
    chars: usize,
    /// Sum of the `chars` of every chunk merged into this one, with no credit for overlap.
    chars_sum: usize,
    /// At least one chunk was merged into this one.
    merged: bool,
}

fn pieces_of(chunks: &[Chunk]) -> Vec<Piece> {
    chunks
        .iter()
        .map(|c| {
            let chars = c.text.chars().count();
            Piece {
                pos: c.pos,
                bytes: c.text.len(),
                chars,
                chars_sum: chars,
                merged: false,
            }
        })
        .collect()
}

/// Merge `b` (the later chunk) into `a`: the union of the two windows, counting their overlap once.
fn merge(a: Piece, b: Piece) -> Piece {
    let overlap_bytes = (a.pos + a.bytes).saturating_sub(b.pos).min(b.bytes);
    // Characters in the overlap, scaled from bytes (exact for ASCII, close otherwise).
    let overlap_chars = (overlap_bytes * b.chars).checked_div(b.bytes).unwrap_or(0);
    Piece {
        pos: a.pos,
        bytes: a.bytes + b.bytes - overlap_bytes,
        chars: a.chars + b.chars - overlap_chars,
        chars_sum: a.chars_sum + b.chars_sum,
        merged: true,
    }
}

/// Merge every chunk below `min` characters into the previous chunk, or into the next one when it
/// is the first. A single chunk is returned unchanged.
fn merge_small(pieces: &[Piece], min: usize) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::with_capacity(pieces.len());
    let mut lead_small = false;
    for &p in pieces {
        if out.is_empty() {
            lead_small = p.chars < min;
            out.push(p);
        } else if lead_small {
            // Only the first chunk can be waiting here: it takes the second chunk whatever its size.
            out[0] = merge(out[0], p);
            lead_small = false;
        } else if p.chars < min {
            let last = out.len() - 1;
            out[last] = merge(out[last], p);
        } else {
            out.push(p);
        }
    }
    out
}

/// Whether `chars` characters are estimated to exceed the embedding token limit.
fn over_token_limit(chars: usize) -> bool {
    chars as f64 / CHARS_PER_TOKEN > TOKEN_LIMIT
}

/// What the merge at one minimum does, summed over documents.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ThresholdStats {
    /// Chunks below the minimum, merged or not.
    below_min: usize,
    /// Documents whose only chunk is below the minimum: nothing to merge it into.
    unmergeable_docs: usize,
    docs_changed: usize,
    chunks_removed: usize,
    /// Chunks that absorbed at least one other chunk.
    merged_chunks: usize,
    largest_merged: usize,
    largest_merged_sum: usize,
    over_chunk_size: usize,
    over_token_limit: usize,
}

impl ThresholdStats {
    fn of_doc(pieces: &[Piece], min: usize) -> Self {
        let after = merge_small(pieces, min);
        let mut s = Self {
            below_min: pieces.iter().filter(|p| p.chars < min).count(),
            unmergeable_docs: usize::from(pieces.len() == 1 && pieces[0].chars < min),
            docs_changed: usize::from(after.len() != pieces.len()),
            chunks_removed: pieces.len() - after.len(),
            ..Self::default()
        };
        for p in after.iter().filter(|p| p.merged) {
            s.merged_chunks += 1;
            s.largest_merged = s.largest_merged.max(p.chars);
            s.largest_merged_sum = s.largest_merged_sum.max(p.chars_sum);
            s.over_chunk_size += usize::from(p.chars > CHUNK_SIZE_CHARS);
            s.over_token_limit += usize::from(over_token_limit(p.chars));
        }
        s
    }

    fn absorb(&mut self, o: &Self) {
        self.below_min += o.below_min;
        self.unmergeable_docs += o.unmergeable_docs;
        self.docs_changed += o.docs_changed;
        self.chunks_removed += o.chunks_removed;
        self.merged_chunks += o.merged_chunks;
        self.largest_merged = self.largest_merged.max(o.largest_merged);
        self.largest_merged_sum = self.largest_merged_sum.max(o.largest_merged_sum);
        self.over_chunk_size += o.over_chunk_size;
        self.over_token_limit += o.over_token_limit;
    }
}

/// Totals for one set of documents: all of them, or one collection.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Tally {
    docs: usize,
    chunks: usize,
    by_min: [ThresholdStats; MINIMUMS.len()],
}

impl Tally {
    fn add_doc(&mut self, pieces: &[Piece]) {
        self.docs += 1;
        self.chunks += pieces.len();
        for (stats, &min) in self.by_min.iter_mut().zip(&MINIMUMS) {
            stats.absorb(&ThresholdStats::of_doc(pieces, min));
        }
    }
}

fn pct(n: usize, of: usize) -> String {
    if of == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", 100.0 * n as f64 / of as f64)
    }
}

fn render_tally(out: &mut String, t: &Tally) {
    let _ = writeln!(out, "  documents: {}   chunks: {}", t.docs, t.chunks);
    for (s, &min) in t.by_min.iter().zip(&MINIMUMS) {
        let _ = writeln!(
            out,
            "  minimum {min} chars: {} chunks below ({} of chunks), {} single-chunk docs below (not mergeable)\n    \
             docs changed {} ({} of docs), chunks removed {} ({} of chunks), merged chunks {}\n    \
             largest merged {} chars (no-overlap-credit upper bound {}), merged > {} chars: {}, \
             merged > {} tokens (estimate): {}",
            s.below_min,
            pct(s.below_min, t.chunks),
            s.unmergeable_docs,
            s.docs_changed,
            pct(s.docs_changed, t.docs),
            s.chunks_removed,
            pct(s.chunks_removed, t.chunks),
            s.merged_chunks,
            s.largest_merged,
            s.largest_merged_sum,
            CHUNK_SIZE_CHARS,
            s.over_chunk_size,
            TOKEN_LIMIT,
            s.over_token_limit,
        );
    }
}

fn render(index: &Path, total: &Tally, per_collection: &BTreeMap<String, Tally>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "chunk-merge measurement (#126), measurement only");
    let _ = writeln!(out, "index: {}", index.display());
    let _ = writeln!(
        out,
        "chunker: size {CHUNK_SIZE_CHARS}, overlap {CHUNK_OVERLAP_CHARS}, window {CHUNK_WINDOW_CHARS}; \
         AST chunking compiled: {}",
        ast_chunking_compiled()
    );
    let _ = writeln!(
        out,
        "token estimate: chars / {CHARS_PER_TOKEN} (an estimate, not a tokenizer); \
         limit {TOKEN_LIMIT} tokens is about {} chars",
        (TOKEN_LIMIT * CHARS_PER_TOKEN) as usize
    );
    let _ = writeln!(
        out,
        "\nall collections (distinct documents by content hash)"
    );
    render_tally(&mut out, total);
    for (name, tally) in per_collection {
        let _ = writeln!(out, "\ncollection {name}");
        render_tally(&mut out, tally);
    }
    out
}

// ── Index reading ────────────────────────────────────────────────────────────

/// Chunk every distinct active document of the index at `path`. Read-only.
fn measure(path: &Path) -> Result<(Tally, BTreeMap<String, Tally>)> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {} read-only", path.display()))?;
    let mut stmt = conn.prepare(
        "SELECT d.collection, d.path, d.hash, c.doc \
         FROM documents d JOIN content c ON c.hash = d.hash \
         WHERE d.active = 1 ORDER BY d.collection, d.path",
    )?;
    let mut rows = stmt.query([])?;

    let mut total = Tally::default();
    let mut per_collection: BTreeMap<String, Tally> = BTreeMap::new();
    let mut by_hash: HashMap<String, Vec<Piece>> = HashMap::new();
    let mut seen_in_collection: HashSet<(String, String)> = HashSet::new();
    while let Some(row) = rows.next()? {
        let collection: String = row.get(0)?;
        let doc_path: String = row.get(1)?;
        let hash: String = row.get(2)?;
        let doc: String = row.get(3)?;
        if !by_hash.contains_key(&hash) {
            let pieces = pieces_of(&chunk_document_for_path(&doc_path, &doc));
            total.add_doc(&pieces);
            by_hash.insert(hash.clone(), pieces);
        }
        if seen_in_collection.insert((collection.clone(), hash.clone())) {
            per_collection
                .entry(collection)
                .or_default()
                .add_doc(&by_hash[&hash]);
        }
    }
    Ok((total, per_collection))
}

// ── Manual step ──────────────────────────────────────────────────────────────

#[test]
#[ignore = "reads the index named by RQMD_MEASURE_INDEX; run by hand with --nocapture"]
fn chunk_merge_measurement() -> Result<()> {
    let path = std::env::var_os(ENV_INDEX)
        .map(PathBuf::from)
        .with_context(|| format!("set {ENV_INDEX} to the index.sqlite path"))?;
    let (total, per_collection) = measure(&path)?;
    println!("{}", render(&path, &total, &per_collection));
    anyhow::ensure!(total.docs > 0, "the index has no active documents");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(pos: usize, len: usize) -> Piece {
        Piece {
            pos,
            bytes: len,
            chars: len,
            chars_sum: len,
            merged: false,
        }
    }

    /// 3700 bytes with no break point: one 3600 chunk, then a 640 tail that starts 540 back.
    fn doc_with_tiny_tail() -> String {
        "x".repeat(CHUNK_SIZE_CHARS + 100)
    }

    #[test]
    fn splitter_leaves_a_short_tail_for_a_document_just_over_the_chunk_size() {
        let chunks = chunk_document_for_path("a.md", &doc_with_tiny_tail());
        let lens: Vec<usize> = chunks.iter().map(|c| c.text.len()).collect();
        assert_eq!(lens, vec![CHUNK_SIZE_CHARS, CHUNK_OVERLAP_CHARS + 100]);
    }

    #[test]
    fn a_tail_below_the_minimum_merges_into_the_previous_chunk() {
        let pieces = pieces_of(&chunk_document_for_path("a.md", &doc_with_tiny_tail()));
        // 640 chars: above 200 and 400, below 800.
        let below_800 = ThresholdStats::of_doc(&pieces, 800);
        assert_eq!(below_800.below_min, 1);
        assert_eq!(below_800.docs_changed, 1);
        assert_eq!(below_800.chunks_removed, 1);
        assert_eq!(below_800.merged_chunks, 1);
        // The union of the two windows is the whole document, not 3600 + 640.
        assert_eq!(below_800.largest_merged, CHUNK_SIZE_CHARS + 100);
        assert_eq!(
            below_800.largest_merged_sum,
            CHUNK_SIZE_CHARS + CHUNK_OVERLAP_CHARS + 100
        );
        assert_eq!(below_800.over_chunk_size, 1);
        assert_eq!(below_800.over_token_limit, 0);
        for min in [200, 400] {
            let s = ThresholdStats::of_doc(&pieces, min);
            assert_eq!(s.below_min, 0, "min {min}");
            assert_eq!(s.docs_changed, 0, "min {min}");
        }
    }

    #[test]
    fn a_small_first_chunk_merges_into_the_next() {
        let merged = merge_small(&[piece(0, 100), piece(100, 3000)], 200);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            (merged[0].pos, merged[0].chars, merged[0].merged),
            (0, 3100, true)
        );
    }

    #[test]
    fn a_small_middle_chunk_merges_into_the_previous_one() {
        let merged = merge_small(&[piece(0, 3000), piece(3000, 100), piece(3100, 3000)], 200);
        let lens: Vec<usize> = merged.iter().map(|p| p.chars).collect();
        assert_eq!(lens, vec![3100, 3000]);
    }

    #[test]
    fn consecutive_small_chunks_collapse_into_one() {
        let merged = merge_small(&[piece(0, 3000), piece(3000, 50), piece(3050, 50)], 200);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].chars, 3100);
    }

    #[test]
    fn a_single_small_chunk_has_no_neighbour_and_is_counted_apart() {
        let s = ThresholdStats::of_doc(&[piece(0, 50)], 200);
        assert_eq!(s.below_min, 1);
        assert_eq!(s.unmergeable_docs, 1);
        assert_eq!(s.docs_changed, 0);
        assert_eq!(s.chunks_removed, 0);
        assert_eq!(s.merged_chunks, 0);
    }

    #[test]
    fn nothing_changes_when_no_chunk_is_below_the_minimum() {
        let pieces = [piece(0, 3600), piece(3060, 900)];
        assert_eq!(merge_small(&pieces, 800), pieces.to_vec());
        assert_eq!(ThresholdStats::of_doc(&pieces, 800).below_min, 0);
    }

    #[test]
    fn a_merged_chunk_over_the_estimated_token_limit_is_counted() {
        // 7200 / 3.5 = 2057 tokens, over 2044; 7000 / 3.5 = 2000, under.
        assert!(over_token_limit(7200));
        assert!(!over_token_limit(7000));
        let s = ThresholdStats::of_doc(&[piece(0, 7100), piece(7100, 100)], 200);
        assert_eq!(s.largest_merged, 7200);
        assert_eq!(s.over_token_limit, 1);
        assert_eq!(s.over_chunk_size, 1);
    }

    #[test]
    fn pieces_count_characters_not_bytes() {
        let chunks = vec![Chunk {
            text: "é".repeat(10),
            pos: 4,
        }];
        let p = pieces_of(&chunks);
        assert_eq!((p[0].pos, p[0].bytes, p[0].chars), (4, 20, 10));
    }

    #[test]
    fn tallies_add_up_across_documents() {
        let mut t = Tally::default();
        t.add_doc(&[piece(0, 3600), piece(3060, 640)]);
        t.add_doc(&[piece(0, 100)]);
        assert_eq!((t.docs, t.chunks), (2, 3));
        let s800 = &t.by_min[2];
        assert_eq!(s800.below_min, 2);
        assert_eq!(s800.unmergeable_docs, 1);
        assert_eq!(s800.docs_changed, 1);
        assert_eq!(s800.chunks_removed, 1);
    }

    /// Build a tiny index with the two tables the measurement reads.
    fn synthetic_index(dir: &Path, docs: &[(&str, &str, &str, &str, i64)]) -> PathBuf {
        let path = dir.join("index.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE content (hash TEXT PRIMARY KEY, doc TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE documents (id INTEGER PRIMARY KEY AUTOINCREMENT, collection TEXT NOT NULL,
               path TEXT NOT NULL, title TEXT NOT NULL, hash TEXT NOT NULL, created_at TEXT NOT NULL,
               modified_at TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1, raw TEXT NOT NULL DEFAULT '',
               UNIQUE(collection, path));",
        )
        .unwrap();
        for &(collection, doc_path, hash, text, active) in docs {
            conn.execute(
                "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, '')",
                (hash, text),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO documents (collection, path, title, hash, created_at, modified_at, active)
                 VALUES (?1, ?2, '', ?3, '', '', ?4)",
                (collection, doc_path, hash, active),
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn measure_reads_active_documents_once_per_hash_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let tail = doc_with_tiny_tail();
        let path = synthetic_index(
            dir.path(),
            &[
                ("notes", "a.md", "h-tail", &tail, 1),
                ("notes", "b.md", "h-short", "tiny", 1),
                // Same text as a.md, another collection: counted once overall, once per collection.
                ("work", "a.md", "h-tail", &tail, 1),
                // Inactive documents are not measured.
                ("work", "old.md", "h-old", "stale", 0),
            ],
        );
        let before = std::fs::read(&path).unwrap();
        let (total, per) = measure(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);

        assert_eq!((total.docs, total.chunks), (2, 3));
        assert_eq!(total.by_min[2].docs_changed, 1);
        assert_eq!(total.by_min[2].unmergeable_docs, 1);
        assert_eq!(
            per.keys().map(String::as_str).collect::<Vec<_>>(),
            ["notes", "work"]
        );
        assert_eq!((per["notes"].docs, per["notes"].chunks), (2, 3));
        assert_eq!((per["work"].docs, per["work"].chunks), (1, 2));
    }

    #[test]
    fn measure_fails_on_a_missing_index_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sqlite");
        assert!(measure(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn the_report_names_every_minimum_collection_and_the_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let tail = doc_with_tiny_tail();
        let path = synthetic_index(dir.path(), &[("notes", "a.md", "h", &tail, 1)]);
        let (total, per) = measure(&path).unwrap();
        let report = render(&path, &total, &per);
        for needle in [
            "minimum 200 chars",
            "minimum 400 chars",
            "minimum 800 chars",
            "collection notes",
            "documents: 1   chunks: 2",
            "(an estimate, not a tokenizer)",
            "largest merged 3700 chars",
        ] {
            assert!(report.contains(needle), "missing {needle:?} in:\n{report}");
        }
    }
}

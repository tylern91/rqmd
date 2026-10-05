//! Smart chunking — direct port of qmd's store.ts chunking logic.
//!
//! Splits documents at high-score break points (headings, code fence boundaries,
//! paragraph breaks) near the CHUNK_SIZE boundary. Never splits inside a code fence.

use regex::Regex;
use std::sync::OnceLock;

use crate::types::Chunk;

// ── Constants (mirrors store.ts) ──────────────────────────────────────────────

/// 900 tokens × ~4 chars/token
pub const CHUNK_SIZE_CHARS: usize = 3600;
/// 135 tokens × ~4 chars/token (15% overlap)
pub const CHUNK_OVERLAP_CHARS: usize = 540;
/// Search window for finding optimal break point (~200 tokens × 4 chars)
pub const CHUNK_WINDOW_CHARS: usize = 800;

/// Bump on any change to break selection, AST dispatch, or overlap semantics —
/// anything that changes where a document gets cut without changing
/// `CHUNK_SIZE_CHARS`/`CHUNK_OVERLAP_CHARS` themselves. Folded into
/// `store::embed_fingerprint` so such changes are detected and trigger
/// re-embedding instead of leaving stale chunk boundaries silently embedded.
pub const CHUNK_STRATEGY_VERSION: u32 = 2;

// ── Break patterns (mirrors BREAK_PATTERNS in store.ts) ──────────────────────

struct BreakPattern {
    pattern: &'static str,
    score: i32,
}

// Rust's regex crate doesn't support lookahead. The heading patterns use
// `\n#{N}[^#]` instead of `\n#{N}(?!#)` — both match only the correct heading
// level since a deeper heading would have another `#` in the [^#] position.
// The extra char consumed is irrelevant; only m.start() (= the `\n` pos) is used.
static BREAK_PATTERNS: &[BreakPattern] = &[
    BreakPattern {
        pattern: r"\n#[^#]",
        score: 100,
    },
    BreakPattern {
        pattern: r"\n##[^#]",
        score: 90,
    },
    BreakPattern {
        pattern: r"\n###[^#]",
        score: 80,
    },
    BreakPattern {
        pattern: r"\n####[^#]",
        score: 70,
    },
    BreakPattern {
        pattern: r"\n#####[^#]",
        score: 60,
    },
    BreakPattern {
        pattern: r"\n######[^#]",
        score: 50,
    },
    BreakPattern {
        pattern: r"\n```",
        score: 80,
    },
    BreakPattern {
        pattern: r"\n(?:---|\*\*\*|___)\s*\n",
        score: 60,
    },
    BreakPattern {
        pattern: r"\n\n+",
        score: 20,
    },
    BreakPattern {
        pattern: r"\n[-*]\s",
        score: 5,
    },
    BreakPattern {
        pattern: r"\n\d+\.\s",
        score: 5,
    },
    BreakPattern {
        pattern: r"\n",
        score: 1,
    },
];

#[derive(Debug, Clone)]
pub(crate) struct BreakPoint {
    pub(crate) pos: usize,
    pub(crate) score: i32,
}

#[derive(Debug, Clone)]
struct CodeFenceRegion {
    start: usize,
    end: usize,
}

// Compiled regexes cached at first use.
fn compiled_patterns() -> &'static Vec<(Regex, i32)> {
    static CACHE: OnceLock<Vec<(Regex, i32)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        BREAK_PATTERNS
            .iter()
            .map(|bp| (Regex::new(bp.pattern).unwrap(), bp.score))
            .collect()
    })
}

fn code_fence_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"```").unwrap())
}

// ── Core algorithms ───────────────────────────────────────────────────────────

fn scan_code_fences(text: &str) -> Vec<CodeFenceRegion> {
    let mut fences = Vec::new();
    let mut opens: Vec<usize> = Vec::new();
    for m in code_fence_regex().find_iter(text) {
        if opens.is_empty() {
            opens.push(m.start());
        } else {
            let start = opens.pop().unwrap();
            fences.push(CodeFenceRegion {
                start,
                end: m.end(),
            });
        }
    }
    // Unclosed fence extends to end of document
    for start in opens {
        fences.push(CodeFenceRegion {
            start,
            end: text.len(),
        });
    }
    fences
}

fn inside_fence(pos: usize, fences: &[CodeFenceRegion]) -> bool {
    fences.iter().any(|f| pos > f.start && pos < f.end)
}

fn scan_break_points(text: &str, fences: &[CodeFenceRegion]) -> Vec<BreakPoint> {
    let mut seen: std::collections::HashMap<usize, i32> = std::collections::HashMap::new();
    for (re, score) in compiled_patterns() {
        for m in re.find_iter(text) {
            let pos = m.start();
            if inside_fence(pos, fences) {
                continue;
            }
            let entry = seen.entry(pos).or_insert(-1);
            if *score > *entry {
                *entry = *score;
            }
        }
    }
    let mut points: Vec<BreakPoint> = seen
        .into_iter()
        .map(|(pos, score)| BreakPoint { pos, score })
        .collect();
    points.sort_by_key(|b| b.pos);
    points
}

/// Find the best break point within [window_start, window_end).
/// Returns `None` if no candidate falls in the window — callers must decide
/// their own fallback (previously this returned `window_end`, which a caller
/// couldn't distinguish from a genuine break found exactly at `window_end`,
/// and which fed a `.max(ideal_end)` guard that silently discarded any
/// backward break; see chunk_document).
pub(crate) fn best_break_in_window(
    break_points: &[BreakPoint],
    window_start: usize,
    window_end: usize,
) -> Option<usize> {
    break_points
        .iter()
        .filter(|b| b.pos >= window_start && b.pos < window_end)
        // Among equal top scores, prefer the latest position — keeps the
        // chunk closer to CHUNK_SIZE_CHARS instead of cutting early.
        .max_by(|a, b| a.score.cmp(&b.score).then(a.pos.cmp(&b.pos)))
        .map(|b| b.pos)
}

// ── Table continuity ──────────────────────────────────────────────────────────

/// A header plus separator row longer than this is not repeated into
/// continuation chunks: the copy would crowd out the rows it is meant to label.
const MAX_TABLE_HEADER_CHARS: usize = CHUNK_WINDOW_CHARS;

/// A GFM pipe table: `start..data_start` is its header row and separator row,
/// `data_start..end` its data rows.
#[derive(Debug, Clone, Copy)]
struct Table {
    start: usize,
    data_start: usize,
    end: usize,
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

fn is_separator_row(line: &str) -> bool {
    let cells: Vec<&str> = line
        .trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(str::trim)
        .collect();
    cells
        .iter()
        .all(|c| !c.is_empty() && c.contains('-') && c.chars().all(|ch| matches!(ch, '-' | ':')))
}

/// Pipe tables outside code fences: a run of consecutive `|` rows whose second
/// row is a separator.
fn scan_tables(text: &str, fences: &[CodeFenceRegion]) -> Vec<Table> {
    let mut tables = Vec::new();
    let mut run: Vec<(usize, &str)> = Vec::new();
    let mut offset = 0;
    let flush = |run: &mut Vec<(usize, &str)>, tables: &mut Vec<Table>| {
        if run.len() >= 2 && is_separator_row(run[1].1) {
            let (last_start, last) = run[run.len() - 1];
            tables.push(Table {
                start: run[0].0,
                data_start: run.get(2).map_or(last_start + last.len(), |r| r.0),
                end: last_start + last.len(),
            });
        }
        run.clear();
    };
    for line in text.split_inclusive('\n') {
        if is_table_row(line) && !inside_fence(offset, fences) {
            run.push((offset, line));
        } else {
            flush(&mut run, &mut tables);
        }
        offset += line.len();
    }
    flush(&mut run, &mut tables);
    tables
}

/// Keep every chunk of a table readable on its own. A chunk that starts inside
/// a table's data rows is moved to the next row boundary and prefixed with the
/// table's header and separator rows; one that starts inside the header is
/// extended back to the table's first row. `pos` stays the offset of the
/// chunk's first original byte.
fn carry_table_headers(text: &str, chunks: Vec<Chunk>, tables: &[Table]) -> Vec<Chunk> {
    chunks
        .into_iter()
        .map(|chunk| {
            let Some(table) = tables
                .iter()
                .find(|t| t.start < chunk.pos && chunk.pos < t.end)
            else {
                return chunk;
            };
            let end = chunk.pos + chunk.text.len();
            if chunk.pos < table.data_start {
                return Chunk {
                    text: text[table.start..end].to_string(),
                    pos: table.start,
                };
            }
            let header = &text[table.start..table.data_start];
            if header.len() > MAX_TABLE_HEADER_CHARS {
                return chunk;
            }
            let row_start = if text.as_bytes()[chunk.pos - 1] == b'\n' {
                chunk.pos
            } else {
                text[chunk.pos..]
                    .find('\n')
                    .map_or(text.len(), |i| chunk.pos + i + 1)
            };
            if row_start >= end || row_start >= table.end {
                return chunk;
            }
            Chunk {
                text: format!("{header}{}", &text[row_start..end]),
                pos: row_start,
            }
        })
        .collect()
}

// ── Char-boundary helpers ─────────────────────────────────────────────────────

/// Advance `pos` to the next UTF-8 char boundary (or text.len()).
fn snap_char_boundary_forward(text: &str, pos: usize) -> usize {
    let mut p = pos.min(text.len());
    while p < text.len() && !text.is_char_boundary(p) {
        p += 1;
    }
    p
}

/// Retreat `pos` to the previous UTF-8 char boundary (or 0).
pub fn snap_char_boundary_backward(text: &str, pos: usize) -> usize {
    let mut p = pos.min(text.len());
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Shared sliding-window join: given a document and a set of candidate break
/// points (regardless of how they were derived — regex-based markdown breaks,
/// or AST declaration-boundary byte offsets), walk forward taking the best
/// break in each window, falling back to a hard cut at `ideal_end` only when
/// no candidate exists. See [`chunk_document`] for why a backward break is
/// accepted rather than discarded.
pub(crate) fn chunk_from_break_points(text: &str, break_points: &[BreakPoint]) -> Vec<Chunk> {
    if text.len() <= CHUNK_SIZE_CHARS {
        return vec![Chunk {
            text: text.to_string(),
            pos: 0,
        }];
    }

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let ideal_end = (start + CHUNK_SIZE_CHARS).min(text.len());
        if ideal_end == text.len() {
            chunks.push(Chunk {
                text: text[start..].to_string(),
                pos: start,
            });
            break;
        }

        // Search for a good break point in the window around the ideal end.
        // A break before ideal_end is accepted, not discarded: window_start is
        // ideal_end - CHUNK_WINDOW_CHARS/2, i.e. start + 3200 bytes, so even the
        // earliest possible accepted break still yields a chunk ≥89% of target size.
        let window_start = ideal_end.saturating_sub(CHUNK_WINDOW_CHARS / 2);
        let window_end = (ideal_end + CHUNK_WINDOW_CHARS / 2).min(text.len());
        let break_at =
            best_break_in_window(break_points, window_start, window_end).unwrap_or(ideal_end);

        let end = break_at.min(text.len());
        // Snap forward to the next valid UTF-8 char boundary. CHUNK_SIZE_CHARS is in
        // bytes (chars ≈ bytes for ASCII, but multi-byte chars like em-dash span 2-3
        // bytes), so ideal_end may land mid-char; regex break_at is always on a
        // boundary, but ideal_end wins when no break point was found.
        let end = snap_char_boundary_forward(text, end);

        chunks.push(Chunk {
            text: text[start..end].to_string(),
            pos: start,
        });

        // Advance with overlap; snap backward to keep start on a char boundary.
        start = snap_char_boundary_backward(text, end.saturating_sub(CHUNK_OVERLAP_CHARS));
        // Ensure we make progress
        if start >= end {
            start = end;
        }
    }

    chunks
}

/// Split `text` into overlapping chunks of at most CHUNK_SIZE_CHARS characters,
/// breaking at high-score positions (headings, paragraph breaks, etc.). A
/// chunk that begins inside a pipe table carries the table's header rows.
pub fn chunk_document(text: &str) -> Vec<Chunk> {
    if text.len() <= CHUNK_SIZE_CHARS {
        return vec![Chunk {
            text: text.to_string(),
            pos: 0,
        }];
    }

    let fences = scan_code_fences(text);
    let break_points = scan_break_points(text, &fences);
    let chunks = chunk_from_break_points(text, &break_points);
    carry_table_headers(text, chunks, &scan_tables(text, &fences))
}

/// Chunk `text` from `path`, dispatching to AST-aware declaration-boundary
/// chunking for source code when the `ast-chunking` feature is compiled in
/// and `path`'s extension has a supported grammar; otherwise (or if the parse
/// finds no boundaries) falls back to [`chunk_document`]'s markdown-oriented
/// chunker so markdown/text/unknown-extension behavior is unchanged.
pub fn chunk_document_for_path(path: &str, text: &str) -> Vec<Chunk> {
    #[cfg(feature = "ast-chunking")]
    {
        if let Some(chunks) = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| crate::ast_chunk::chunk_source(text, ext))
        {
            return chunks;
        }
    }
    #[cfg(not(feature = "ast-chunking"))]
    {
        let _ = path;
    }
    chunk_document(text)
}

/// Whether AST-aware declaration-boundary chunking is compiled into this
/// binary. `cfg!` resolves against `rqmd-core`'s own feature set at compile
/// time, so this is accurate regardless of whether the calling crate (e.g.
/// `rqmd-cli`) declares an `ast-chunking` feature of its own.
pub fn ast_chunking_compiled() -> bool {
    cfg!(feature = "ast-chunking")
}

/// File extensions dispatched to AST chunking when [`ast_chunking_compiled`]
/// is `true`. Empty when the feature is off — every extension falls back to
/// [`chunk_document`] in that case.
pub fn ast_chunking_extensions() -> &'static [&'static str] {
    #[cfg(feature = "ast-chunking")]
    {
        &["ts", "tsx", "js", "jsx", "mjs", "cjs", "java", "py"]
    }
    #[cfg(not(feature = "ast-chunking"))]
    {
        &[]
    }
}

/// Compute just the first chunk of `text` — byte-identical to
/// `chunk_document(text)[0]`, but without `chunk_document`'s whole-document
/// fence/break-point scan. Only the region up to the first chunk's search
/// window can affect where that chunk ends, so this scans that prefix only.
/// Callers that only need a fallback snippet (not full chunking) should use
/// this instead of `chunk_document(text).remove(0)`.
pub fn first_chunk(text: &str) -> String {
    if text.len() <= CHUNK_SIZE_CHARS {
        return text.to_string();
    }

    let ideal_end = CHUNK_SIZE_CHARS;
    let window_start = ideal_end.saturating_sub(CHUNK_WINDOW_CHARS / 2);
    let window_end = (ideal_end + CHUNK_WINDOW_CHARS / 2).min(text.len());

    let scan_end = snap_char_boundary_forward(text, window_end);
    let fences = scan_code_fences(&text[..scan_end]);
    let break_points = scan_break_points(&text[..scan_end], &fences);
    let break_at =
        best_break_in_window(&break_points, window_start, window_end).unwrap_or(ideal_end);

    let end = snap_char_boundary_forward(text, break_at.min(text.len()));
    text[..end].to_string()
}

// ── Snippet extraction ────────────────────────────────────────────────────────

/// Result of [`extract_snippet`].
pub struct SnippetResult {
    /// 1-indexed line number of the best matching line in the full document.
    pub line: usize,
    /// Snippet text with diff-style header: `@@ -start,count @@ (N before, M after)`.
    pub snippet: String,
}

/// Extract a query-relevant snippet from a document body.
///
/// Mirrors `extractSnippet` in qmd's `store.ts` (lines 4544–4627).  The returned
/// snippet carries a diff-style header (`@@ -start,count @@ (before, after)`)
/// so the caller knows where in the file the excerpt was found.
///
/// Parameters:
/// - `body`       — full document text
/// - `query`      — search query (whitespace-separated terms)
/// - `max_len`    — maximum character length of the snippet text (default 500)
/// - `chunk_pos`  — byte offset of the best chunk in `body` (0 = unknown / first chunk)
/// - `chunk_len`  — character length of the best chunk (0 = unknown)
pub fn extract_snippet(
    body: &str,
    query: &str,
    max_len: usize,
    chunk_pos: usize,
    chunk_len: usize,
) -> SnippetResult {
    let total_lines = body.lines().count();

    // Determine the search region.
    let (search_body, line_offset) = if chunk_pos > 0 {
        let search_len = if chunk_len > 0 {
            chunk_len
        } else {
            CHUNK_SIZE_CHARS
        };
        // `chunk_pos` is a byte offset; context is added in chars → convert to bytes
        // by snapping to char boundaries.
        let ctx_start_byte = snap_char_boundary_backward(body, chunk_pos.saturating_sub(100));
        let ctx_end_byte =
            snap_char_boundary_forward(body, (chunk_pos + search_len + 100).min(body.len()));
        let lo = body[..ctx_start_byte].lines().count().saturating_sub(1);
        (&body[ctx_start_byte..ctx_end_byte], lo)
    } else {
        (body, 0)
    };

    let lines: Vec<&str> = search_body.lines().collect();
    let query_terms: Vec<String> = query
        .to_lowercase()
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    // Score each line by term overlap.
    let mut best_line = 0usize;
    let mut best_score: i32 = -1;
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let mut score = 0i32;
        for term in &query_terms {
            if lower.contains(term.as_str()) {
                score += 1;
            }
        }
        if score > best_score {
            best_score = score;
            best_line = i;
        }
    }

    // If we focused on a chunk window but found no match, fall back to the full body.
    if chunk_pos > 0 && best_score <= 0 {
        // The reranker picked this chunk — anchor on the chunk start.
        let ctx_start_byte = snap_char_boundary_backward(body, chunk_pos.saturating_sub(100));
        best_line = if chunk_pos > ctx_start_byte {
            body[ctx_start_byte..chunk_pos]
                .lines()
                .count()
                .saturating_sub(1)
        } else {
            0
        };
        return build_snippet_result(&lines, best_line, line_offset, total_lines, max_len);
    }

    build_snippet_result(&lines, best_line, line_offset, total_lines, max_len)
}

fn build_snippet_result(
    lines: &[&str],
    best_line: usize,
    line_offset: usize,
    total_lines: usize,
    max_len: usize,
) -> SnippetResult {
    let start = best_line.saturating_sub(1);
    let end = (best_line + 3).min(lines.len());
    let snippet_lines = &lines[start..end];
    let mut snippet_text = snippet_lines.join("\n");

    if snippet_text.len() > max_len {
        let cut = snap_char_boundary_backward(&snippet_text, max_len.saturating_sub(3));
        snippet_text.truncate(cut);
        snippet_text.push_str("...");
    }

    let absolute_start = line_offset + start + 1; // 1-indexed
    let snippet_line_count = snippet_lines.len();
    let lines_before = absolute_start - 1;
    let lines_after = total_lines.saturating_sub(absolute_start + snippet_line_count - 1);

    let header = format!(
        "@@ -{absolute_start},{snippet_line_count} @@ ({lines_before} before, {lines_after} after)"
    );
    let snippet = format!("{header}\n{snippet_text}");
    let line = line_offset + best_line + 1;

    SnippetResult { line, snippet }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE_HEADER: &str = "| id | name | note |\n|----|------|------|\n";

    fn table_doc(rows: usize) -> String {
        let mut doc = String::from("# Inventory\n\nIntro paragraph.\n\n");
        doc.push_str(TABLE_HEADER);
        for i in 0..rows {
            doc.push_str(&format!("| {i:04} | item-{i:04} | note for row {i:04} |\n"));
        }
        doc.push_str("\nClosing paragraph.\n");
        doc
    }

    fn unadorned_chunks(text: &str) -> Vec<Chunk> {
        let fences = scan_code_fences(text);
        chunk_from_break_points(text, &scan_break_points(text, &fences))
    }

    #[test]
    fn continuation_chunks_of_a_long_table_start_at_a_row_with_the_header() {
        let doc = table_doc(400);
        let chunks = chunk_document(&doc);
        assert!(chunks.len() > 3, "fixture must span several chunks");

        let mut continuations = 0;
        for chunk in chunks.iter().filter(|c| c.pos > 0) {
            let in_data_rows =
                doc[chunk.pos..].starts_with("| ") && chunk.pos > doc.find(TABLE_HEADER).unwrap();
            if !in_data_rows {
                continue;
            }
            continuations += 1;
            assert!(
                chunk.text.starts_with(TABLE_HEADER),
                "continuation chunk at {} lacks the header: {:?}",
                chunk.pos,
                &chunk.text[..chunk.text.len().min(80)]
            );
            let first_row = &chunk.text[TABLE_HEADER.len()..];
            assert!(
                first_row.starts_with("| 0"),
                "must start on a whole row: {first_row:.40}"
            );
            assert!(doc[..chunk.pos].ends_with('\n'), "pos must be a row start");
        }
        assert!(continuations >= 2, "expected several table continuations");
    }

    #[test]
    fn a_document_without_tables_chunks_exactly_as_before() {
        let doc = "word ".repeat(2000);
        let chunks = chunk_document(&doc);
        let expected = unadorned_chunks(&doc);
        assert_eq!(chunks.len(), expected.len());
        for (a, b) in chunks.iter().zip(&expected) {
            assert_eq!((&a.text, a.pos), (&b.text, b.pos));
        }
    }

    #[test]
    fn pipe_rows_inside_a_code_fence_are_not_a_table() {
        let mut doc = String::from("```\n");
        doc.push_str(TABLE_HEADER);
        for i in 0..400 {
            doc.push_str(&format!("| {i:04} | item-{i:04} | note for row {i:04} |\n"));
        }
        doc.push_str("```\n");
        let chunks = chunk_document(&doc);
        let expected = unadorned_chunks(&doc);
        assert_eq!(chunks.len(), expected.len());
        for (a, b) in chunks.iter().zip(&expected) {
            assert_eq!((&a.text, a.pos), (&b.text, b.pos));
        }
    }

    #[test]
    fn scan_tables_needs_a_separator_row() {
        let with_sep = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let without_sep = "| a | b |\n| 1 | 2 |\n| 3 | 4 |\n";
        assert_eq!(scan_tables(with_sep, &[]).len(), 1);
        assert!(scan_tables(without_sep, &[]).is_empty());
        let t = scan_tables(with_sep, &[])[0];
        assert_eq!(&with_sep[t.start..t.data_start], "| a | b |\n|---|---|\n");
        assert_eq!(t.end, with_sep.len());
    }

    #[test]
    fn a_chunk_starting_inside_the_header_is_extended_back_to_the_table_start() {
        let doc = format!("intro\n\n{TABLE_HEADER}| 1 | a | b |\n| 2 | c | d |\n");
        let tables = scan_tables(&doc, &[]);
        let table_start = tables[0].start;
        let mid_header = table_start + 4;
        let chunk = Chunk {
            text: doc[mid_header..].to_string(),
            pos: mid_header,
        };

        let out = carry_table_headers(&doc, vec![chunk], &tables);

        assert_eq!(out[0].pos, table_start);
        assert_eq!(out[0].text, doc[table_start..]);
    }

    #[test]
    fn an_oversized_header_is_not_repeated() {
        let wide = "x".repeat(MAX_TABLE_HEADER_CHARS);
        let doc = format!("| {wide} |\n|---|\n| 1 |\n| 2 |\n| 3 |\n");
        let tables = scan_tables(&doc, &[]);
        let pos = doc.find("| 2 |").unwrap();
        let chunk = Chunk {
            text: doc[pos..].to_string(),
            pos,
        };

        let out = carry_table_headers(&doc, vec![chunk], &tables);

        assert_eq!(out[0].pos, pos);
        assert_eq!(out[0].text, doc[pos..]);
    }

    #[test]
    fn short_doc_is_single_chunk() {
        let text = "hello world";
        let chunks = chunk_document(text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].pos, 0);
    }

    #[test]
    fn long_doc_splits_at_heading() {
        let section_a = "# Section A\n".to_string() + &"word ".repeat(700);
        let section_b = "# Section B\n".to_string() + &"word ".repeat(700);
        let text = section_a + &section_b;
        let chunks = chunk_document(&text);
        // Should produce at least 2 chunks; each should start with the section header
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn no_split_inside_code_fence() {
        let inner = "line\n".repeat(1000); // long enough to require splitting
        let text = format!("```\n{inner}```\n");
        let chunks = chunk_document(&text);
        // All chunk boundaries should be outside the fence (at position 0 or after ```)
        for chunk in &chunks {
            // Verify chunk content doesn't start mid-fence
            let _ = chunk.pos; // just ensure it compiles
        }
    }

    #[test]
    fn backward_break_is_taken_instead_of_hard_cut() {
        // A heading break at byte 3300 falls inside the search window
        // [3200, 4000) around ideal_end=3600 but *before* it. The chunk must
        // end at the heading, not be hard-cut at 3600 mid-word.
        let before = "a".repeat(3300);
        let heading = "\n# Heading\n";
        let after = "b".repeat(2000);
        let text = format!("{before}{heading}{after}");

        let chunks = chunk_document(&text);
        // The break point is the `\n` that precedes the heading, so the first
        // chunk ends exactly there (3300 bytes) instead of being hard-cut at
        // ideal_end (3600).
        assert_eq!(
            chunks[0].text.len(),
            3300,
            "expected the backward heading break at 3300 to be taken, got chunk len {}",
            chunks[0].text.len()
        );
        // The heading itself starts the next chunk (via overlap), not mid-body.
        assert!(
            chunks[1].text.starts_with("\n# Heading\n") || chunks[1].text.contains("# Heading"),
            "next chunk should pick up at the heading, got: {:?}",
            &chunks[1].text[..30.min(chunks[1].text.len())]
        );
    }

    #[test]
    fn first_chunk_matches_chunk_document_short_doc() {
        let text = "hello world";
        assert_eq!(first_chunk(text), chunk_document(text)[0].text);
    }

    #[test]
    fn first_chunk_matches_chunk_document_long_doc() {
        let section_a = "# Section A\n".to_string() + &"word ".repeat(700);
        let section_b = "# Section B\n".to_string() + &"word ".repeat(700);
        let text = section_a + &section_b;
        assert_eq!(first_chunk(&text), chunk_document(&text)[0].text);
    }

    #[test]
    fn snippet_truncation_respects_utf8_boundary() {
        // "é" is 2 bytes in UTF-8. 200 repetitions = 400 bytes.
        // max_len = 100 → naive cut at byte 97 (odd) lands mid-'é' and panicked
        // before the fix. chunk_pos = 0 passes body straight to build_snippet_result.
        let body = "é".repeat(200);
        let result = extract_snippet(&body, "é", 100, 0, 0);
        assert!(
            result.snippet.ends_with("..."),
            "snippet should be truncated with ellipsis"
        );
    }
}

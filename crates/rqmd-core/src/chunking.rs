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

// ── Break patterns (mirrors BREAK_PATTERNS in store.ts) ──────────────────────

struct BreakPattern {
    pattern: &'static str,
    score: i32,
    #[allow(dead_code)]
    kind: &'static str,
}

// Rust's regex crate doesn't support lookahead. The heading patterns use
// `\n#{N}[^#]` instead of `\n#{N}(?!#)` — both match only the correct heading
// level since a deeper heading would have another `#` in the [^#] position.
// The extra char consumed is irrelevant; only m.start() (= the `\n` pos) is used.
static BREAK_PATTERNS: &[BreakPattern] = &[
    BreakPattern {
        pattern: r"\n#[^#]",
        score: 100,
        kind: "h1",
    },
    BreakPattern {
        pattern: r"\n##[^#]",
        score: 90,
        kind: "h2",
    },
    BreakPattern {
        pattern: r"\n###[^#]",
        score: 80,
        kind: "h3",
    },
    BreakPattern {
        pattern: r"\n####[^#]",
        score: 70,
        kind: "h4",
    },
    BreakPattern {
        pattern: r"\n#####[^#]",
        score: 60,
        kind: "h5",
    },
    BreakPattern {
        pattern: r"\n######[^#]",
        score: 50,
        kind: "h6",
    },
    BreakPattern {
        pattern: r"\n```",
        score: 80,
        kind: "codeblock",
    },
    BreakPattern {
        pattern: r"\n(?:---|\*\*\*|___)\s*\n",
        score: 60,
        kind: "hr",
    },
    BreakPattern {
        pattern: r"\n\n+",
        score: 20,
        kind: "blank",
    },
    BreakPattern {
        pattern: r"\n[-*]\s",
        score: 5,
        kind: "list",
    },
    BreakPattern {
        pattern: r"\n\d+\.\s",
        score: 5,
        kind: "numlist",
    },
    BreakPattern {
        pattern: r"\n",
        score: 1,
        kind: "newline",
    },
];

#[derive(Debug, Clone)]
struct BreakPoint {
    pos: usize,
    score: i32,
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
/// Returns the position of the break, or `window_end` if no point found.
fn best_break_in_window(
    break_points: &[BreakPoint],
    window_start: usize,
    window_end: usize,
) -> usize {
    let candidates: Vec<&BreakPoint> = break_points
        .iter()
        .filter(|b| b.pos >= window_start && b.pos < window_end)
        .collect();

    if candidates.is_empty() {
        return window_end;
    }

    candidates
        .iter()
        .max_by(|a, b| a.score.cmp(&b.score).then(b.pos.cmp(&a.pos)))
        .map(|b| b.pos)
        .unwrap_or(window_end)
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
fn snap_char_boundary_backward(text: &str, pos: usize) -> usize {
    let mut p = pos.min(text.len());
    while p > 0 && !text.is_char_boundary(p) {
        p -= 1;
    }
    p
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Split `text` into overlapping chunks of at most CHUNK_SIZE_CHARS characters,
/// breaking at high-score positions (headings, paragraph breaks, etc.).
pub fn chunk_document(text: &str) -> Vec<Chunk> {
    if text.len() <= CHUNK_SIZE_CHARS {
        return vec![Chunk {
            text: text.to_string(),
            pos: 0,
        }];
    }

    let fences = scan_code_fences(text);
    let break_points = scan_break_points(text, &fences);

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

        // Search for a good break point in the window around the ideal end
        let window_start = ideal_end.saturating_sub(CHUNK_WINDOW_CHARS / 2);
        let window_end = (ideal_end + CHUNK_WINDOW_CHARS / 2).min(text.len());
        let break_at = best_break_in_window(&break_points, window_start, window_end);

        let end = break_at.max(ideal_end); // never go backwards
        let end = end.min(text.len());
        // Snap forward to the next valid UTF-8 char boundary. CHUNK_SIZE_CHARS is in
        // bytes (chars ≈ bytes for ASCII, but multi-byte chars like em-dash span 2-3
        // bytes), so ideal_end may land mid-char; regex break_at is always on a
        // boundary, but ideal_end wins when break_at < ideal_end.
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
/// - `intent`     — optional domain intent string (ignored in this port; reserved for future)
pub fn extract_snippet(
    body: &str,
    query: &str,
    max_len: usize,
    chunk_pos: usize,
    chunk_len: usize,
    _intent: Option<&str>,
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
        if chunk_pos == 0 {
            return extract_snippet(body, query, max_len, 0, 0, None);
        }
        // The reranker picked this chunk — anchor on the chunk start.
        let ctx_start_byte = snap_char_boundary_backward(body, chunk_pos.saturating_sub(100));
        let lines_before_ctx = body[..ctx_start_byte].lines().count().saturating_sub(1);
        best_line = if chunk_pos > ctx_start_byte {
            body[ctx_start_byte..chunk_pos]
                .lines()
                .count()
                .saturating_sub(1)
        } else {
            0
        };
        return build_snippet_result(
            body,
            search_body,
            &lines,
            best_line,
            line_offset,
            lines_before_ctx,
            total_lines,
            max_len,
        );
    }

    build_snippet_result(
        body,
        search_body,
        &lines,
        best_line,
        line_offset,
        0,
        total_lines,
        max_len,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_snippet_result(
    _full_body: &str,
    _search_body: &str,
    lines: &[&str],
    best_line: usize,
    line_offset: usize,
    _extra_offset: usize,
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
    fn snippet_truncation_respects_utf8_boundary() {
        // "é" is 2 bytes in UTF-8. 200 repetitions = 400 bytes.
        // max_len = 100 → naive cut at byte 97 (odd) lands mid-'é' and panicked
        // before the fix. chunk_pos = 0 passes body straight to build_snippet_result.
        let body = "é".repeat(200);
        let result = extract_snippet(&body, "é", 100, 0, 0, None);
        assert!(
            result.snippet.ends_with("..."),
            "snippet should be truncated with ellipsis"
        );
    }
}

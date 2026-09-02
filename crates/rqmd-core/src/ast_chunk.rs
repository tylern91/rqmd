//! AST-aware chunking for source code, gated behind the `ast-chunking` feature.
//!
//! Chunks at function/method/class declaration boundaries instead of the
//! markdown-oriented regex breaks in [`crate::chunking`] — code rarely has a
//! blank-line or heading break near the 3600-byte target, so the character
//! chunker degrades to near-arbitrary cuts (see the plan's 0.3% high-quality-
//! discard measurement for source code). Reuses the same windowed-join
//! primitive as the markdown chunker ([`chunk_from_break_points`]) so an
//! oversized single function still gets a reasonable backward-break split
//! instead of a hard cut mid-token.

use crate::chunking::{BreakPoint, chunk_from_break_points};
use crate::types::Chunk;

/// Declaration node kinds treated as chunk boundaries, per language. Kept
/// deliberately narrow (function/method/class-ish units only) rather than
/// exhaustive — false negatives just fall through to a hard cut via the
/// windowed join, false positives would fragment chunks too finely.
fn boundary_kinds(ext: &str) -> Option<(Language, &'static [&'static str])> {
    match ext {
        "ts" => Some((Language::Typescript, JS_TS_KINDS)),
        "tsx" => Some((Language::Tsx, JS_TS_KINDS)),
        "js" | "jsx" | "mjs" | "cjs" => Some((Language::Javascript, JS_TS_KINDS)),
        "java" => Some((Language::Java, JAVA_KINDS)),
        "py" => Some((Language::Python, PY_KINDS)),
        _ => None,
    }
}

enum Language {
    Typescript,
    Tsx,
    Javascript,
    Java,
    Python,
}

static JS_TS_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "method_definition",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
];
static JAVA_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "method_declaration",
    "constructor_declaration",
];
static PY_KINDS: &[&str] = &["function_definition", "class_definition"];

/// Cap on tree-sitter parse time, guarding against pathological input hanging
/// the parser indefinitely (tree-sitter has no built-in limit otherwise).
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000;

/// Parse `text` as `ext` and chunk at declaration boundaries. Returns `None`
/// when `ext` has no supported grammar, parsing fails, or the parse yields no
/// boundary nodes at all (e.g. an empty or trivial file) — callers fall back
/// to [`crate::chunking::chunk_document`] in every `None` case.
pub(crate) fn chunk_source(text: &str, ext: &str) -> Option<Vec<Chunk>> {
    let (language, kinds) = boundary_kinds(ext)?;
    let ts_language = match language {
        Language::Typescript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Javascript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
    };

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts_language).ok()?;

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_micros(PARSE_TIMEOUT_MICROS);
    let mut cancel_if_overdue =
        |_state: &tree_sitter::ParseState| std::time::Instant::now() >= deadline;
    let mut text_provider = |offset: usize, _point: tree_sitter::Point| -> &[u8] {
        text.as_bytes().get(offset..).unwrap_or(&[])
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut cancel_if_overdue);
    let tree = parser.parse_with_options(&mut text_provider, None, Some(options))?;

    let mut positions = Vec::new();
    collect_boundaries(tree.root_node(), kinds, &mut positions);
    if positions.is_empty() {
        return None;
    }
    positions.sort_unstable();
    positions.dedup();

    let break_points: Vec<BreakPoint> = positions
        .into_iter()
        .map(|pos| BreakPoint { pos, score: 100 })
        .collect();
    Some(chunk_from_break_points(text, &break_points))
}

fn collect_boundaries(node: tree_sitter::Node, kinds: &[&str], out: &mut Vec<usize>) {
    // Iterative traversal with an explicit heap stack — recursion here would
    // let adversarial/deeply-nested source overflow the call stack, which
    // Rust cannot catch (it aborts the process).
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if kinds.contains(&node.kind()) {
            out.push(node.start_byte());
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(chunk_source("hello", "md").is_none());
        assert!(chunk_source("hello", "").is_none());
    }

    #[test]
    fn python_chunks_at_function_boundaries() {
        // `def alpha` body sized so `def beta`'s start byte (~3217) falls inside
        // the [3200, 4000) search window around the first chunk's ideal_end
        // (3600) — mirrors chunking::backward_break_is_taken_instead_of_hard_cut.
        let body_a = "    pass\n".repeat(356);
        let body_b = "    pass\n".repeat(500);
        let text = format!("def alpha():\n{body_a}def beta():\n{body_b}");
        let chunks = chunk_source(&text, "py").expect("python grammar should parse");
        assert!(chunks.len() >= 2, "expected a split between alpha and beta");
        // The break is taken exactly at `def beta`'s start byte (3217), not a
        // hard cut at ideal_end (3600) — overlap then pulls chunk[1]'s start
        // back by CHUNK_OVERLAP_CHARS, so chunk[1] contains but doesn't begin
        // with "def beta".
        assert_eq!(chunks[0].text.len(), 3217);
        assert!(chunks[0].text.trim_end().ends_with("pass"));
        assert!(chunks[1].text.contains("def beta"));
    }

    #[test]
    fn trivial_source_with_no_declarations_falls_back() {
        // A file with no function/class nodes (just statements) has no
        // boundaries to find — the caller must fall back to chunk_document.
        assert!(chunk_source("x = 1\ny = 2\n", "py").is_none());
    }
}

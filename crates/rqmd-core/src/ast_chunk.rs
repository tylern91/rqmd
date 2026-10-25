//! AST-aware chunking for source code, gated behind the `ast-chunking` feature
//! (or any single `ast-<language>` feature).
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

/// One supported source language: the file extensions it owns, its grammar,
/// the declaration node kinds that start a chunk, and the fingerprint marker
/// its vectors carry. Adding a language is one entry here plus one `ast-<name>`
/// cargo feature (and its optional grammar dependency) in `rqmd-core`.
///
/// Boundary kinds are kept deliberately narrow (function/method/class-ish
/// units only) rather than exhaustive: false negatives just fall through to a
/// hard cut via the windowed join, false positives would fragment chunks too
/// finely.
struct Language {
    extensions: &'static [&'static str],
    grammar: fn() -> tree_sitter::Language,
    kinds: &'static [&'static str],
    /// Sibling node kinds (comments, Rust attributes) that sit on their own
    /// line directly above a declaration and travel with it, so a chunk does
    /// not end on the doc comment of the next declaration. Empty for the
    /// languages that predate the registry, whose chunk boundaries must not
    /// move.
    leading: &'static [&'static str],
    /// Stop descending below a matched node, so a block nested in a block
    /// (a Terraform `lifecycle {}` inside a `resource {}`) is not a boundary.
    outermost_only: bool,
    /// Text appended to the embed fingerprint of every file of this language.
    /// The languages that were AST-chunked before the registry existed all use
    /// [`LEGACY_MARKER`], which keeps their fingerprints, and so their stored
    /// vectors, valid. A language added later carries its own marker, so
    /// changing one grammar re-embeds only that language's files.
    marker: &'static str,
}

/// The marker shared by the languages that predate the registry
/// (ts/tsx/js/jsx/mjs/cjs/java/py). Never change it: it is part of the
/// fingerprint stored with every vector of those files.
#[cfg_attr(
    not(any(
        feature = "ast-typescript",
        feature = "ast-javascript",
        feature = "ast-java",
        feature = "ast-python"
    )),
    allow(dead_code)
)]
const LEGACY_MARKER: &str = "ast_chunking:1";

/// The languages compiled into this build, in registry order.
static REGISTRY: &[Language] = &[
    #[cfg(feature = "ast-typescript")]
    Language {
        extensions: &["ts"],
        grammar: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        kinds: JS_TS_KINDS,
        leading: &[],
        outermost_only: false,
        marker: LEGACY_MARKER,
    },
    #[cfg(feature = "ast-typescript")]
    Language {
        extensions: &["tsx"],
        grammar: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        kinds: JS_TS_KINDS,
        leading: &[],
        outermost_only: false,
        marker: LEGACY_MARKER,
    },
    #[cfg(feature = "ast-javascript")]
    Language {
        extensions: &["js", "jsx", "mjs", "cjs"],
        grammar: || tree_sitter_javascript::LANGUAGE.into(),
        kinds: JS_TS_KINDS,
        leading: &[],
        outermost_only: false,
        marker: LEGACY_MARKER,
    },
    #[cfg(feature = "ast-java")]
    Language {
        extensions: &["java"],
        grammar: || tree_sitter_java::LANGUAGE.into(),
        kinds: JAVA_KINDS,
        leading: &[],
        outermost_only: false,
        marker: LEGACY_MARKER,
    },
    #[cfg(feature = "ast-python")]
    Language {
        extensions: &["py"],
        grammar: || tree_sitter_python::LANGUAGE.into(),
        kinds: PY_KINDS,
        leading: &[],
        outermost_only: false,
        marker: LEGACY_MARKER,
    },
    #[cfg(feature = "ast-go")]
    Language {
        extensions: &["go"],
        grammar: || tree_sitter_go::LANGUAGE.into(),
        kinds: GO_KINDS,
        leading: &["comment"],
        outermost_only: false,
        marker: "ast_chunking:1\nast_language:go:1",
    },
    #[cfg(feature = "ast-rust")]
    Language {
        extensions: &["rs"],
        grammar: || tree_sitter_rust::LANGUAGE.into(),
        kinds: RUST_KINDS,
        leading: &["line_comment", "block_comment", "attribute_item"],
        outermost_only: false,
        marker: "ast_chunking:1\nast_language:rust:1",
    },
    #[cfg(feature = "ast-csharp")]
    Language {
        extensions: &["cs"],
        grammar: || tree_sitter_c_sharp::LANGUAGE.into(),
        kinds: CSHARP_KINDS,
        leading: &["comment"],
        outermost_only: false,
        marker: "ast_chunking:1\nast_language:csharp:1",
    },
    #[cfg(feature = "ast-kotlin")]
    Language {
        extensions: &["kt", "kts"],
        grammar: || tree_sitter_kotlin_ng::LANGUAGE.into(),
        kinds: KOTLIN_KINDS,
        leading: &["line_comment", "block_comment"],
        outermost_only: false,
        marker: "ast_chunking:1\nast_language:kotlin:1",
    },
    #[cfg(feature = "ast-swift")]
    Language {
        extensions: &["swift"],
        grammar: || tree_sitter_swift::LANGUAGE.into(),
        kinds: SWIFT_KINDS,
        leading: &["comment", "multiline_comment"],
        outermost_only: false,
        marker: "ast_chunking:1\nast_language:swift:1",
    },
    #[cfg(feature = "ast-hcl")]
    Language {
        extensions: &["tf", "tfvars", "hcl"],
        grammar: || tree_sitter_hcl::LANGUAGE.into(),
        kinds: HCL_KINDS,
        leading: &["comment"],
        outermost_only: true,
        marker: "ast_chunking:1\nast_language:hcl:1",
    },
    #[cfg(feature = "ast-sql")]
    Language {
        extensions: &["sql"],
        grammar: || tree_sitter_sequel::LANGUAGE.into(),
        kinds: SQL_KINDS,
        leading: &["comment", "marginalia"],
        outermost_only: true,
        marker: "ast_chunking:1\nast_language:sql:1",
    },
];

fn language_for(ext: &str) -> Option<&'static Language> {
    REGISTRY.iter().find(|l| l.extensions.contains(&ext))
}

/// Every extension with a compiled grammar, in registry order.
pub(crate) fn extensions() -> Vec<&'static str> {
    REGISTRY
        .iter()
        .flat_map(|l| l.extensions.iter().copied())
        .collect()
}

/// The fingerprint marker of `ext`'s language, or `None` without a grammar.
pub(crate) fn fingerprint_marker(ext: &str) -> Option<&'static str> {
    language_for(ext).map(|l| l.marker)
}

/// Extensions grouped by fingerprint marker, in registry order: each group's
/// files share one embed fingerprint.
pub(crate) fn extensions_by_marker() -> Vec<(&'static str, Vec<&'static str>)> {
    let mut groups: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
    for language in REGISTRY {
        match groups.iter_mut().find(|(m, _)| *m == language.marker) {
            Some((_, exts)) => exts.extend(language.extensions),
            None => groups.push((language.marker, language.extensions.to_vec())),
        }
    }
    groups
}

#[cfg(any(feature = "ast-typescript", feature = "ast-javascript"))]
static JS_TS_KINDS: &[&str] = &[
    "function_declaration",
    "generator_function_declaration",
    "class_declaration",
    "method_definition",
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
];
#[cfg(feature = "ast-java")]
static JAVA_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "method_declaration",
    "constructor_declaration",
];
#[cfg(feature = "ast-python")]
static PY_KINDS: &[&str] = &["function_definition", "class_definition"];
#[cfg(feature = "ast-go")]
static GO_KINDS: &[&str] = &[
    "function_declaration",
    "method_declaration",
    "type_declaration",
];
#[cfg(feature = "ast-rust")]
static RUST_KINDS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "union_item",
    "trait_item",
    "impl_item",
    "mod_item",
    "type_item",
    "macro_definition",
];
#[cfg(feature = "ast-csharp")]
static CSHARP_KINDS: &[&str] = &[
    "class_declaration",
    "interface_declaration",
    "struct_declaration",
    "enum_declaration",
    "record_declaration",
    "method_declaration",
    "constructor_declaration",
];
#[cfg(feature = "ast-kotlin")]
static KOTLIN_KINDS: &[&str] = &[
    "class_declaration",
    "object_declaration",
    "function_declaration",
    "secondary_constructor",
];
#[cfg(feature = "ast-swift")]
static SWIFT_KINDS: &[&str] = &[
    "class_declaration",
    "protocol_declaration",
    "function_declaration",
    "init_declaration",
    "deinit_declaration",
];
#[cfg(feature = "ast-hcl")]
static HCL_KINDS: &[&str] = &["block"];
#[cfg(feature = "ast-sql")]
static SQL_KINDS: &[&str] = &["statement"];

/// Cap on tree-sitter parse time, guarding against pathological input hanging
/// the parser indefinitely (tree-sitter has no built-in limit otherwise).
const PARSE_TIMEOUT_MICROS: u64 = 5_000_000;

/// Parse `text` as `ext` and chunk at declaration boundaries. Returns `None`
/// when `ext` has no supported grammar, parsing fails, or the parse yields no
/// boundary nodes at all (e.g. an empty or trivial file) — callers fall back
/// to [`crate::chunking::chunk_document`] in every `None` case.
pub(crate) fn chunk_source(text: &str, ext: &str) -> Option<Vec<Chunk>> {
    let language = language_for(ext)?;
    let ts_language = (language.grammar)();

    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&ts_language).ok()?;

    let deadline =
        std::time::Instant::now() + std::time::Duration::from_micros(PARSE_TIMEOUT_MICROS);
    let mut cancel_if_overdue = |_state: &tree_sitter::ParseState| {
        if std::time::Instant::now() >= deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let mut text_provider = |offset: usize, _point: tree_sitter::Point| -> &[u8] {
        text.as_bytes().get(offset..).unwrap_or(&[])
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut cancel_if_overdue);
    let tree = parser.parse_with_options(&mut text_provider, None, Some(options))?;

    let mut positions = Vec::new();
    collect_boundaries(tree.root_node(), language, &mut positions);
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

fn collect_boundaries(root: tree_sitter::Node, language: &Language, out: &mut Vec<usize>) {
    if language.kinds.contains(&root.kind()) {
        out.push(root.start_byte());
        if language.outermost_only {
            return;
        }
    }
    // Iterative traversal with an explicit heap stack — recursion here would
    // let adversarial/deeply-nested source overflow the call stack, which
    // Rust cannot catch (it aborts the process).
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let mut cursor = node.walk();
        let children: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
        for (at, child) in children.iter().enumerate() {
            if language.kinds.contains(&child.kind()) {
                out.push(boundary_start(&children, at, language.leading));
                if language.outermost_only {
                    continue;
                }
            }
            stack.push(*child);
        }
    }
}

/// Where a chunk may begin for the declaration `siblings[at]`: the start of its
/// own text, or of the run of `leading` siblings that sit on their own lines
/// just above it. A comment that shares a line with the code before it belongs
/// to that code, so the run stops there. Works on the sibling list the caller
/// already holds: asking tree-sitter for each previous sibling would rescan the
/// parent's children every time, quadratic for a flat file of many statements.
fn boundary_start(siblings: &[tree_sitter::Node], at: usize, leading: &[&str]) -> usize {
    let mut start = siblings[at].start_byte();
    let mut i = at;
    while i > 0 && leading.contains(&siblings[i - 1].kind()) {
        let comment = siblings[i - 1];
        // A node that ends in column 0 swallowed its own newline (a Rust line
        // comment does), so what follows it starts on a fresh line.
        let trails_previous_code = i >= 2 && {
            let end = siblings[i - 2].end_position();
            end.column != 0 && end.row == comment.start_position().row
        };
        if trails_previous_code {
            break;
        }
        start = comment.start_byte();
        i -= 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(chunk_source("hello", "md").is_none());
        assert!(chunk_source("hello", "").is_none());
    }

    #[cfg(feature = "ast-python")]
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

    #[cfg(feature = "ast-python")]
    #[test]
    fn trivial_source_with_no_declarations_falls_back() {
        // A file with no function/class nodes (just statements) has no
        // boundaries to find — the caller must fall back to chunk_document.
        assert!(chunk_source("x = 1\ny = 2\n", "py").is_none());
    }

    /// Chunks `first + second_head + body + tail` and asserts the first chunk
    /// ends exactly where `second_head` begins. `first` is sized so that
    /// offset falls inside the search window around the 3,600-byte target but
    /// not on it, so a hard cut at the target would land elsewhere: only a
    /// break taken at a syntax node puts the boundary there.
    #[cfg(any(
        feature = "ast-go",
        feature = "ast-rust",
        feature = "ast-csharp",
        feature = "ast-kotlin",
        feature = "ast-swift",
        feature = "ast-hcl"
    ))]
    fn assert_splits_before(ext: &str, head: &str, line: &str, tail: &str, second_head: &str) {
        let mut first = head.to_string();
        while first.len() + tail.len() < 3400 {
            first.push_str(line);
        }
        first.push_str(tail);
        let text = format!("{first}{second_head}{}{tail}", line.repeat(300));
        let chunks = chunk_source(&text, ext)
            .unwrap_or_else(|| panic!("the {ext} grammar should find declarations"));
        assert!(chunks.len() >= 2, "{ext}: expected a split");
        assert_eq!(
            chunks[0].text.len(),
            first.len(),
            "{ext}: the first chunk must end where the second declaration begins"
        );
        assert!(chunks[1].text.contains(second_head), "{ext}");
    }

    #[cfg(feature = "ast-go")]
    #[test]
    fn go_chunks_at_declaration_boundaries() {
        assert_splits_before(
            "go",
            "package main\n\nfunc alpha() {\n",
            "\t_ = 1\n",
            "}\n\n",
            "func beta() {\n",
        );
    }

    #[cfg(feature = "ast-go")]
    #[test]
    fn go_trailing_comment_stays_with_the_code_it_follows() {
        let first = format!("func alpha() {{\n{}}}\n\n", "\t_ = 1\n".repeat(485));
        let trailing = "var x = 1 // set once\n";
        let text = format!(
            "{first}{trailing}func beta() {{\n{}}}\n",
            "\t_ = 1\n".repeat(300)
        );
        let chunks = chunk_source(&text, "go").expect("go grammar should parse");
        assert_eq!(chunks[0].text.len(), first.len() + trailing.len());
    }

    #[cfg(feature = "ast-rust")]
    #[test]
    fn rust_chunks_at_item_boundaries_and_keeps_docs_with_their_item() {
        assert_splits_before(
            "rs",
            "fn alpha() {\n",
            "    let _ = 1;\n",
            "}\n\n",
            "/// Docs for beta.\n#[inline]\nfn beta() {\n",
        );
    }

    #[cfg(feature = "ast-csharp")]
    #[test]
    fn csharp_chunks_at_type_boundaries() {
        assert_splits_before("cs", "enum Alpha {\n", "    A,\n", "}\n\n", "enum Beta {\n");
    }

    #[cfg(feature = "ast-kotlin")]
    #[test]
    fn kotlin_chunks_at_declaration_boundaries() {
        for ext in ["kt", "kts"] {
            assert_splits_before(
                ext,
                "fun alpha() {\n",
                "    val x = 1\n",
                "}\n\n",
                "fun beta() {\n",
            );
        }
    }

    #[cfg(feature = "ast-swift")]
    #[test]
    fn swift_chunks_at_declaration_boundaries() {
        assert_splits_before(
            "swift",
            "func alpha() {\n",
            "    let x = 1\n",
            "}\n\n",
            "func beta() {\n",
        );
    }

    #[cfg(feature = "ast-hcl")]
    #[test]
    fn hcl_chunks_at_top_level_blocks() {
        for ext in ["tf", "tfvars", "hcl"] {
            assert_splits_before(
                ext,
                "resource \"aws_s3_bucket\" \"alpha\" {\n",
                "  bucket = \"x\"\n",
                "}\n\n",
                "resource \"aws_s3_bucket\" \"beta\" {\n",
            );
        }
    }

    #[cfg(feature = "ast-hcl")]
    #[test]
    fn hcl_nested_blocks_are_not_boundaries() {
        // The only block start inside the search window is the nested
        // `lifecycle`; the next top-level block begins past it. If the nested
        // block were a boundary the chunk would end there, at about 3,500
        // bytes, instead of at the 3,600-byte target.
        let mut first = String::from("resource \"a\" \"alpha\" {\n");
        while first.len() < 3500 {
            first.push_str("  bucket = \"x\"\n");
        }
        first.push_str("  lifecycle {\n    prevent_destroy = true\n  }\n");
        while first.len() < 4200 {
            first.push_str("  bucket = \"x\"\n");
        }
        first.push_str("}\n\n");
        let text = format!("{first}resource \"a\" \"beta\" {{\n  bucket = \"x\"\n}}\n");
        let chunks = chunk_source(&text, "tf").expect("hcl grammar should parse");
        assert_eq!(chunks[0].text.len(), 3600);
    }

    #[cfg(feature = "ast-sql")]
    #[test]
    fn sql_chunks_between_statements() {
        let line = "INSERT INTO t VALUES (1);\n";
        let text = line.repeat(300);
        let chunks = chunk_source(&text, "sql").expect("sql grammar should parse");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].text.len() % line.len(), 0, "a statement was cut");
        assert!(chunks[0].text.ends_with(";\n"));
    }

    #[cfg(feature = "ast-sql")]
    #[test]
    fn sql_function_body_is_not_split() {
        // The body, with its own semicolons, spans the search window; the
        // chunk must end before the function, not inside it.
        let table = format!(
            "CREATE TABLE t (\n{}  last integer\n);\n",
            "  c integer,\n".repeat(250)
        );
        let function = format!(
            "CREATE FUNCTION f() RETURNS integer AS $$ BEGIN {} RETURN 1; END; $$ LANGUAGE plpgsql;\n",
            "PERFORM 1; ".repeat(100)
        );
        let text = format!("{table}{function}SELECT 1;\n");
        let chunks = chunk_source(&text, "sql").expect("sql grammar should parse");
        assert!(chunks.len() >= 2);
        assert_eq!(chunks[0].text.len(), table.len());
    }

    #[test]
    fn languages_that_predate_the_registry_share_one_marker() {
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "java", "py"] {
            if let Some(marker) = fingerprint_marker(ext) {
                assert_eq!(marker, LEGACY_MARKER, "{ext}");
            }
        }
        assert_eq!(LEGACY_MARKER, "ast_chunking:1");
    }

    #[test]
    fn every_extension_has_one_owner_and_one_marker() {
        let exts = extensions();
        let unique: std::collections::HashSet<_> = exts.iter().collect();
        assert_eq!(unique.len(), exts.len(), "an extension is registered twice");

        let grouped: Vec<&str> = extensions_by_marker()
            .into_iter()
            .flat_map(|(_, e)| e)
            .collect();
        let mut grouped_sorted = grouped.clone();
        let mut exts_sorted = exts.clone();
        grouped_sorted.sort_unstable();
        exts_sorted.sort_unstable();
        assert_eq!(grouped_sorted, exts_sorted);
    }

    #[test]
    fn each_added_language_has_its_own_marker() {
        let added = [
            ("go", "go"),
            ("rs", "rs"),
            ("cs", "cs"),
            ("kt", "kts"),
            ("swift", "swift"),
            ("tf", "tfvars"),
            ("tf", "hcl"),
            ("sql", "sql"),
        ];
        for (a, b) in added {
            if let (Some(ma), Some(mb)) = (fingerprint_marker(a), fingerprint_marker(b)) {
                assert_eq!(ma, mb, "{a} and {b} are one language");
                assert_ne!(ma, LEGACY_MARKER, "{a} must not reuse the legacy marker");
            }
        }
        let markers: Vec<_> = extensions_by_marker().into_iter().map(|(m, _)| m).collect();
        let unique: std::collections::HashSet<_> = markers.iter().collect();
        assert_eq!(unique.len(), markers.len());
    }
}

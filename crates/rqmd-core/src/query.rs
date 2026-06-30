//! Query syntax parser — implements the QMD query language (docs/SYNTAX.md).
//!
//! A QMD query is either:
//!   - An **expand query**: a single line (or `expand: text`) passed to the
//!     generation model, which emits `lex:`/`vec:`/`hyde:` expansions.
//!   - A **query document**: one or more typed lines (`lex:`, `vec:`, `hyde:`,
//!     optionally one `intent:` line) that bypass the generation model and
//!     route each sub-query directly to its search method.

use crate::types::QueryType;

/// One typed sub-query within a query document.
#[derive(Debug, Clone)]
pub struct SubQuery {
    pub qtype: QueryType,
    pub text: String,
}

/// The result of parsing a raw query string.
#[derive(Debug, Clone)]
pub struct ParsedQuery {
    /// Optional context/disambiguation hint.  Present in typed query documents
    /// (via an `intent:` line) or supplied externally via the `--intent` flag.
    pub intent: Option<String>,
    /// Typed sub-queries from a query document.  Non-empty ⇒ bypass expansion.
    pub subqueries: Vec<SubQuery>,
    /// The text to expand when `subqueries` is empty.  Present iff this is an
    /// expand query (plain line or `expand: text`).
    pub expand_text: Option<String>,
}

/// Parse a raw query string per the QMD query syntax.
///
/// Rules:
/// - Blank lines are ignored.
/// - Leading/trailing whitespace is trimmed from every line.
/// - If any non-blank line starts with a typed prefix (`lex:`, `vec:`, `hyde:`,
///   or `intent:`), the entire query is treated as a **query document**.
/// - Otherwise the entire query (minus an optional `expand:` prefix) is the
///   text to expand.
/// - In a query document, an inner `expand:` line is silently ignored per spec.
pub fn parse_query(raw: &str) -> ParsedQuery {
    let lines: Vec<&str> = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();

    if lines.is_empty() {
        return ParsedQuery {
            intent: None,
            subqueries: vec![],
            expand_text: Some(String::new()),
        };
    }

    // Detect query-document mode: any line starts with a recognized typed prefix.
    let is_typed_doc = lines.iter().any(|l| {
        l.starts_with("lex:")
            || l.starts_with("vec:")
            || l.starts_with("hyde:")
            || l.starts_with("intent:")
    });

    if is_typed_doc {
        return parse_query_document(&lines);
    }

    // Expand mode: strip optional `expand:` prefix.
    let text = if lines.len() == 1 {
        let line = lines[0];
        if let Some(rest) = line.strip_prefix("expand:") {
            rest.trim().to_string()
        } else {
            line.to_string()
        }
    } else {
        // Multi-line but no typed prefixes — treat the whole thing as expand text.
        lines.join(" ")
    };

    ParsedQuery {
        intent: None,
        subqueries: vec![],
        expand_text: Some(text),
    }
}

fn parse_query_document(lines: &[&str]) -> ParsedQuery {
    let mut intent: Option<String> = None;
    let mut subqueries: Vec<SubQuery> = Vec::new();

    for line in lines {
        if let Some(text) = line.strip_prefix("intent:") {
            // At most one intent line; later ones are ignored.
            if intent.is_none() {
                intent = Some(text.trim().to_string());
            }
        } else if let Some(text) = line.strip_prefix("lex:") {
            subqueries.push(SubQuery {
                qtype: QueryType::Lex,
                text: text.trim().to_string(),
            });
        } else if let Some(text) = line.strip_prefix("vec:") {
            subqueries.push(SubQuery {
                qtype: QueryType::Vec,
                text: text.trim().to_string(),
            });
        } else if let Some(text) = line.strip_prefix("hyde:") {
            subqueries.push(SubQuery {
                qtype: QueryType::Hyde,
                text: text.trim().to_string(),
            });
        }
        // `expand:` inside a query document is ignored (per SYNTAX.md constraint).
    }

    ParsedQuery {
        intent,
        subqueries,
        expand_text: None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_single_line() {
        let p = parse_query("how does auth work");
        assert!(p.subqueries.is_empty());
        assert_eq!(p.expand_text.as_deref(), Some("how does auth work"));
        assert!(p.intent.is_none());
    }

    #[test]
    fn explicit_expand_prefix() {
        let p = parse_query("expand: error handling best practices");
        assert!(p.subqueries.is_empty());
        assert_eq!(
            p.expand_text.as_deref(),
            Some("error handling best practices")
        );
    }

    #[test]
    fn typed_lex_vec() {
        let raw = "lex: auth token\nvec: how does authentication work";
        let p = parse_query(raw);
        assert_eq!(p.subqueries.len(), 2);
        assert!(matches!(p.subqueries[0].qtype, QueryType::Lex));
        assert_eq!(p.subqueries[0].text, "auth token");
        assert!(matches!(p.subqueries[1].qtype, QueryType::Vec));
        assert_eq!(p.subqueries[1].text, "how does authentication work");
        assert!(p.expand_text.is_none());
        assert!(p.intent.is_none());
    }

    #[test]
    fn typed_with_intent() {
        let raw = "intent: web page load times\nlex: performance\nvec: how to improve performance";
        let p = parse_query(raw);
        assert_eq!(p.intent.as_deref(), Some("web page load times"));
        assert_eq!(p.subqueries.len(), 2);
    }

    #[test]
    fn typed_with_hyde() {
        let raw = "lex: rate limiter\nhyde: The rate limiter uses a token bucket algorithm";
        let p = parse_query(raw);
        assert_eq!(p.subqueries.len(), 2);
        assert!(matches!(p.subqueries[1].qtype, QueryType::Hyde));
    }

    #[test]
    fn lex_negation_passthrough() {
        // Lex syntax (-word, "phrase") is preserved for Tantivy downstream.
        let raw = r#"lex: auth -oauth "machine learning""#;
        let p = parse_query(raw);
        assert_eq!(p.subqueries.len(), 1);
        assert_eq!(p.subqueries[0].text, r#"auth -oauth "machine learning""#);
    }

    #[test]
    fn expand_inside_doc_is_ignored() {
        // expand: inside a query document must be silently dropped.
        let raw = "lex: something\nexpand: ignored";
        let p = parse_query(raw);
        // "expand:" line is not a typed prefix → not collected into subqueries.
        // But since the doc mode is triggered by "lex:", "expand:" is ignored.
        assert_eq!(p.subqueries.len(), 1);
        assert!(p.expand_text.is_none());
    }

    #[test]
    fn blank_lines_ignored() {
        let raw = "\n  lex: auth\n\n  vec: authentication\n";
        let p = parse_query(raw);
        assert_eq!(p.subqueries.len(), 2);
    }

    #[test]
    fn empty_input() {
        let p = parse_query("");
        assert!(p.subqueries.is_empty());
        assert_eq!(p.expand_text.as_deref(), Some(""));
    }

    #[test]
    fn duplicate_intent_keeps_first() {
        let raw = "intent: first\nintent: second\nlex: auth";
        let p = parse_query(raw);
        assert_eq!(p.intent.as_deref(), Some("first"));
    }
}

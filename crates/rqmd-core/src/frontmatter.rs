//! Frontmatter scan shared by document preparation and full-text rebuilds.
//!
//! A hand-rolled scalar scan over `---`-fenced YAML frontmatter — deliberately
//! NOT a full YAML parser. Obsidian frontmatter can contain arbitrary nested
//! structures (multi-line block scalars, nested maps) that a strict parser
//! would reject outright, turning a file that indexes fine today into a hard
//! failure on some unrelated field this code doesn't even care about. This
//! scan only looks for what indexing actually needs (`title`, `tags`,
//! `aliases`) and silently ignores everything else it doesn't recognize,
//! degrading to "no title found" rather than erroring.

pub struct Frontmatter {
    pub title: Option<String>,
    /// Flattened tag/alias scalar values, in document order.
    pub terms: Vec<String>,
}

/// Split `raw` into (frontmatter, remaining body). Returns `(None, raw)`
/// when `raw` doesn't open with a `---` fence on its own first line, or
/// the fence is never closed.
pub fn split(raw: &str) -> (Option<Frontmatter>, &str) {
    if raw.lines().next() != Some("---") {
        return (None, raw);
    }
    let after_open = match raw.find('\n') {
        Some(i) => i + 1,
        None => return (None, raw),
    };

    // Scan line-by-line from just past the opening fence for a closing "---".
    let mut pos = after_open;
    loop {
        let line_end = raw[pos..].find('\n').map(|i| pos + i);
        let line = match line_end {
            Some(end) => &raw[pos..end],
            None => &raw[pos..], // final line, no trailing newline
        };
        if line.trim_end_matches('\r') == "---" {
            let block = &raw[after_open..pos];
            let body_start = match line_end {
                Some(end) => end + 1,
                None => raw.len(),
            };
            return (Some(parse_block(block)), &raw[body_start..]);
        }
        match line_end {
            Some(end) => pos = end + 1,
            None => return (None, raw), // reached EOF without a closing fence
        }
    }
}

/// Scan the frontmatter block's lines for top-level `key: value` pairs,
/// tracking `tags:`/`aliases:` block-list continuations (`  - item` lines
/// with no inline value on the key line itself).
fn parse_block(block: &str) -> Frontmatter {
    let mut title = None;
    let mut terms = Vec::new();
    let mut list_key: Option<&str> = None;

    for line in block.lines() {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.trim().is_empty() {
            continue;
        }

        // An indented "- item" line continues the previous list key, if any;
        // any other indented line is a continuation we don't understand and
        // simply skip, without resetting list_key (a blank scalar continuation
        // shouldn't end an in-progress list).
        if trimmed.starts_with(' ') || trimmed.starts_with('-') {
            if let Some(key) = list_key {
                let t = trimmed.trim();
                if let Some(item) = t.strip_prefix('-') {
                    let value = unquote(item.trim());
                    if !value.is_empty() && (key == "tags" || key == "aliases") {
                        terms.push(value);
                    }
                }
            }
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            list_key = None;
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if value.is_empty() {
            // Opens a block list on the following indented lines.
            list_key = Some(key);
            continue;
        }
        list_key = None;

        match key {
            "title" if title.is_none() => {
                let v = unquote(value);
                if !v.is_empty() {
                    title = Some(v);
                }
            }
            "tags" | "aliases" => {
                for item in flow_list_items(value) {
                    let v = unquote(item);
                    if !v.is_empty() {
                        terms.push(v);
                    }
                }
            }
            _ => {}
        }
    }

    Frontmatter { title, terms }
}

/// Split a `[a, b, c]` flow list into its items, or treat the whole value
/// as a single scalar item when it isn't bracketed (`tags: solo-tag`).
fn flow_list_items(value: &str) -> Vec<&str> {
    match value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        Some(inner) => inner
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec![value],
    }
}

/// Strip a single layer of matching `'...'`/`"..."` quoting, if present.
fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

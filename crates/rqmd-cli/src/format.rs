use rqmd_core::{extract_snippet, SearchResult};

pub type Format = str;

// ── ANSI helpers ──────────────────────────────────────────────────────────────

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
#[allow(dead_code)]
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

fn ansi_enabled() -> bool {
    std::env::var("NO_COLOR").is_err() && atty_stdout()
}

/// Check if stdout is a TTY — used to gate colors and interactive progress.
fn atty_stdout() -> bool {
    libc_isatty(1)
}

/// Check if stderr is a TTY — used to gate progress bar output.
pub fn atty_stderr() -> bool {
    libc_isatty(2)
}

// We link against libc via the standard library — just call isatty directly.
#[cfg(unix)]
fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) != 0 }
}

#[cfg(not(unix))]
fn libc_isatty(_fd: i32) -> bool {
    false
}

/// Terminal column count for stderr, via TIOCGWINSZ ioctl.
/// Returns None if not a tty or the syscall fails.
#[cfg(unix)]
pub fn term_width() -> Option<usize> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    extern "C" {
        fn ioctl(fd: i32, request: std::os::raw::c_ulong, argp: *mut Winsize) -> i32;
    }
    // TIOCGWINSZ constant differs by platform.
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x4008_7468;
    #[cfg(not(target_os = "macos"))]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { ioctl(2, TIOCGWINSZ, &mut ws as *mut Winsize) };
    if rc == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
pub fn term_width() -> Option<usize> {
    None
}

/// Fit `s` to exactly `width` visible columns.
///
/// Truncates if longer, space-pads if shorter (so a shorter new line erases a
/// longer previous one).  ANSI SGR escapes (`\x1b[…m`) are copied through and
/// do NOT count toward width.  A trailing reset is appended so a mid-color
/// truncation cannot bleed.  Char-based — no byte slicing, UTF-8 safe.
pub fn fit_to_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            out.push(c);
            if c == 'm' {
                in_escape = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_escape = true;
            out.push(c);
            continue;
        }
        if visible >= width {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out.push_str("\x1b[0m");
    if visible < width {
        out.push_str(&" ".repeat(width - visible));
    }
    out
}

fn b(s: &str) -> String {
    if ansi_enabled() {
        format!("{BOLD}{s}{RESET}")
    } else {
        s.to_string()
    }
}
fn dim(s: &str) -> String {
    if ansi_enabled() {
        format!("{DIM}{s}{RESET}")
    } else {
        s.to_string()
    }
}
fn cyan(s: &str) -> String {
    if ansi_enabled() {
        format!("{CYAN}{s}{RESET}")
    } else {
        s.to_string()
    }
}
#[allow(dead_code)]
fn yellow(s: &str) -> String {
    if ansi_enabled() {
        format!("{YELLOW}{s}{RESET}")
    } else {
        s.to_string()
    }
}
#[allow(dead_code)]
fn green(s: &str) -> String {
    if ansi_enabled() {
        format!("{GREEN}{s}{RESET}")
    } else {
        s.to_string()
    }
}

// ── Score formatting (mirrors qmd's formatScore) ──────────────────────────────

/// Format a score as a right-aligned percentage with color coding.
/// Mirrors `formatScore` in qmd.ts (lines 2025–2031).
pub fn format_score(score: f32) -> String {
    let pct = (score * 100.0).round() as i64;
    let pct_str = format!("{pct:>3}%");
    if !ansi_enabled() {
        return pct_str;
    }
    if score >= 0.7 {
        format!("{GREEN}{pct_str}{RESET}")
    } else if score >= 0.4 {
        format!("{YELLOW}{pct_str}{RESET}")
    } else {
        format!("{DIM}{pct_str}{RESET}")
    }
}

// ── Progress bar (mirrors qmd's renderProgressBar) ────────────────────────────

/// Render a filled/empty progress bar.
/// Mirrors `renderProgressBar` in qmd.ts (lines 1791–1796).
pub fn render_progress_bar(percent: f64, width: usize) -> String {
    let filled = ((percent / 100.0) * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    "█".repeat(filled) + &"░".repeat(empty)
}

// ── ETA formatting (mirrors qmd's formatETA) ──────────────────────────────────

/// Format elapsed/remaining seconds as "Xs", "Xm Ys", or "Xh Ym".
/// Mirrors `formatETA` in qmd.ts (line 305).
pub fn format_eta(secs: f64) -> String {
    let s = secs.round() as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    }
}

// ── Time-ago formatting (mirrors qmd's formatTimeAgo) ────────────────────────

/// Parse a fixed-format RFC-3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) to Unix seconds.
/// Returns None on parse failure (rather than crashing).
fn parse_rfc3339_utc(ts: &str) -> Option<u64> {
    // Expected format: "2024-06-29T12:34:56Z" (20 chars).
    let ts = ts.trim_end_matches('Z');
    let (date, time) = ts.split_once('T')?;
    let mut dp = date.splitn(3, '-');
    let y: u64 = dp.next()?.parse().ok()?;
    let mo: u64 = dp.next()?.parse().ok()?;
    let d: u64 = dp.next()?.parse().ok()?;
    let mut tp = time.splitn(3, ':');
    let h: u64 = tp.next()?.parse().ok()?;
    let m: u64 = tp.next()?.parse().ok()?;
    let s: u64 = tp.next().unwrap_or("0").parse().ok()?;

    // Days from civil epoch (inverse of format_rfc3339 in rqmd-core).
    let (y, mo) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y / 400;
    let yoe = y % 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe;
    let days = days.checked_sub(719_468)?;
    Some(days * 86_400 + h * 3600 + m * 60 + s)
}

/// Format a stored RFC-3339 UTC timestamp as a human-readable "time ago" string.
/// Mirrors qmd's `formatTimeAgo` (qmd.ts:369).
pub fn format_time_ago(rfc3339: &str) -> String {
    let then = match parse_rfc3339_utc(rfc3339) {
        Some(s) => s,
        None => return rfc3339.to_string(),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if now < then {
        return "just now".to_string();
    }
    let diff = now - then;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86_400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 7 * 86_400 {
        format!("{}d ago", diff / 86_400)
    } else {
        // Fall back to the date portion of the timestamp
        rfc3339.get(..10).unwrap_or(rfc3339).to_string()
    }
}

// ── Term highlighting (mirrors qmd's highlightTerms) ──────────────────────────

/// Highlight query terms (len ≥ 3) in bold+yellow when colors are enabled.
/// Mirrors `highlightTerms` in qmd.ts (lines 2013–2022).
pub fn highlight_terms(text: &str, query: &str) -> String {
    if !ansi_enabled() {
        return text.to_string();
    }
    let terms: Vec<&str> = query
        .split_whitespace()
        .filter(|t| t.chars().count() >= 3)
        .collect();
    if terms.is_empty() {
        return text.to_string();
    }
    let mut result = text.to_string();
    for term in &terms {
        let lower_term = term.to_lowercase();
        // Simple case-insensitive replacement: find and wrap each occurrence.
        let mut out = String::with_capacity(result.len());
        let lower_result = result.to_lowercase();
        let mut last = 0;
        let mut pos = 0;
        while pos < lower_result.len() {
            if let Some(idx) = lower_result[pos..].find(lower_term.as_str()) {
                let abs = pos + idx;
                out.push_str(&result[last..abs]);
                out.push_str(&format!(
                    "{YELLOW}{BOLD}{}{RESET}",
                    &result[abs..abs + term.len()]
                ));
                last = abs + term.len();
                pos = last;
            } else {
                break;
            }
        }
        out.push_str(&result[last..]);
        result = out;
    }
    result
}

// ── Format dispatch ───────────────────────────────────────────────────────────

pub fn print_results(results: &[SearchResult], format: &Format, show_full: bool, query: &str) {
    match format {
        "json" => print_json(results, show_full, query),
        "csv" => print_csv(results, show_full, query),
        "md" | "markdown" => print_markdown(results, show_full, query),
        "xml" => print_xml(results, show_full, query),
        "files" => print_files(results),
        _ => print_cli(results, show_full, query),
    }
}

// ── CLI (colored terminal) ────────────────────────────────────────────────────
// Mirrors `outputResults` CLI branch in qmd.ts (lines 2212–2296).

fn print_cli(results: &[SearchResult], show_full: bool, query: &str) {
    if results.is_empty() {
        // qmd prints to stdout (not stderr) for empty results
        println!("No results found.");
        return;
    }
    for (i, r) in results.iter().enumerate() {
        let snippet_info = extract_snippet(
            &r.body,
            query,
            500,
            r.best_chunk_pos,
            r.best_chunk.chars().count(),
            None,
        );

        // Only show :line if a query term matches the snippet body (after the header line).
        let snippet_body_lower = snippet_info
            .snippet
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        let has_match = query
            .split_whitespace()
            .any(|t| !t.is_empty() && snippet_body_lower.contains(&t.to_lowercase()));
        let line_info = if has_match {
            format!(":{}", snippet_info.line)
        } else {
            String::new()
        };

        let docid_str = dim(&format!(" #{}", r.docid));
        // Line 1: path:line #docid
        println!("{}{}{docid_str}", cyan(&r.file), dim(&line_info));

        // Line 2: Title (if present)
        if !r.title.is_empty() {
            println!("{}", b(&format!("Title: {}", r.title)));
        }

        // Line 3: Context (if present)
        if let Some(ref ctx) = r.context {
            if !ctx.is_empty() {
                println!("{}", dim(&format!("Context: {ctx}")));
            }
        }

        // Line 4: Score
        println!("Score: {}", b(&format_score(r.score)));

        // Blank line before snippet
        println!();

        // Snippet (or full body with --full)
        let content = if show_full {
            r.body.as_str()
        } else {
            snippet_info.snippet.as_str()
        };
        let highlighted = highlight_terms(content, query);
        println!("{highlighted}");

        // Double blank line between results (qmd prints "\n\n" between)
        if i < results.len() - 1 {
            println!("\n");
        }
    }
}

// ── JSON ──────────────────────────────────────────────────────────────────────
// Mirrors `outputResults` JSON branch in qmd.ts (lines 2175–2199).

fn print_json(results: &[SearchResult], show_full: bool, query: &str) {
    if results.is_empty() {
        println!("[]");
        return;
    }
    let arr: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            let snippet_info = extract_snippet(
                &r.body,
                query,
                300,
                r.best_chunk_pos,
                r.best_chunk.chars().count(),
                None,
            );
            let score_rounded = (r.score * 100.0).round() / 100.0;
            let mut obj = serde_json::json!({
                "docid": format!("#{}", r.docid),
                "score": score_rounded,
                "file": r.file,
                "line": snippet_info.line,
                "title": r.title,
            });
            if let Some(ref ctx) = r.context {
                obj["context"] = serde_json::Value::String(ctx.clone());
            }
            if show_full {
                obj["body"] = serde_json::Value::String(r.body.clone());
            } else {
                obj["snippet"] = serde_json::Value::String(snippet_info.snippet);
            }
            obj
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
}

// ── CSV ───────────────────────────────────────────────────────────────────────
// Mirrors `outputResults` CSV branch in qmd.ts (lines 2326–2347).

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn print_csv(results: &[SearchResult], show_full: bool, query: &str) {
    println!("docid,score,file,title,context,line,snippet");
    for r in results {
        let snippet_info = extract_snippet(
            &r.body,
            query,
            500,
            r.best_chunk_pos,
            r.best_chunk.chars().count(),
            None,
        );
        let content = if show_full {
            &r.body
        } else {
            &snippet_info.snippet
        };
        let ctx = r.context.as_deref().unwrap_or("");
        println!(
            "#{},{:.4},{},{},{},{},{}",
            r.docid,
            r.score,
            csv_field(&r.file),
            csv_field(&r.title),
            csv_field(ctx),
            snippet_info.line,
            csv_field(content),
        );
    }
}

// ── Markdown ──────────────────────────────────────────────────────────────────
// Mirrors `outputResults` md branch in qmd.ts (lines 2297–2313).

fn print_markdown(results: &[SearchResult], show_full: bool, query: &str) {
    for r in results {
        let heading = if r.title.is_empty() {
            &r.file
        } else {
            &r.title
        };
        let snippet_info = extract_snippet(
            &r.body,
            query,
            500,
            r.best_chunk_pos,
            r.best_chunk.chars().count(),
            None,
        );
        let content = if show_full {
            r.body.trim()
        } else {
            snippet_info.snippet.trim_end()
        };
        let docid_line = format!("**docid:** `#{}`\n", r.docid);
        let ctx_line = r
            .context
            .as_deref()
            .map(|c| format!("**context:** {c}\n"))
            .unwrap_or_default();
        println!(
            "---\n# {heading}\n**file:** `{}`\n{docid_line}{ctx_line}\n{content}\n",
            r.file
        );
    }
}

// ── XML ───────────────────────────────────────────────────────────────────────
// Mirrors `outputResults` xml branch in qmd.ts (lines 2314–2324).

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn print_xml(results: &[SearchResult], show_full: bool, query: &str) {
    if results.is_empty() {
        println!("<results></results>");
        return;
    }
    for r in results {
        let snippet_info = extract_snippet(
            &r.body,
            query,
            500,
            r.best_chunk_pos,
            r.best_chunk.chars().count(),
            None,
        );
        let content = if show_full {
            r.body.as_str()
        } else {
            snippet_info.snippet.as_str()
        };
        let title_attr = if r.title.is_empty() {
            String::new()
        } else {
            format!(" title=\"{}\"", xml_escape(&r.title))
        };
        let ctx_attr = r
            .context
            .as_deref()
            .map(|c| format!(" context=\"{}\"", xml_escape(c)))
            .unwrap_or_default();
        println!(
            "<file docid=\"#{}\" name=\"{}\"{title_attr}{ctx_attr}>\n{}\n</file>\n",
            xml_escape(&r.docid),
            xml_escape(&r.file),
            xml_escape(content),
        );
    }
}

// ── Files ─────────────────────────────────────────────────────────────────────
// Mirrors `outputResults` files branch in qmd.ts (lines 2200–2211).

fn print_files(results: &[SearchResult]) {
    for r in results {
        let ctx = r
            .context
            .as_deref()
            .map(|c| format!(",\"{}\"", c.replace('"', "\"\"")))
            .unwrap_or_default();
        println!("#{},{:.2},{}{ctx}", r.docid, r.score, r.file);
    }
}

// ── Document output (for get / multi-get) ────────────────────────────────────

pub fn print_document(
    file: &str,
    title: &str,
    body: &str,
    format: &Format,
    max_lines: Option<usize>,
    line_numbers: bool,
) {
    let text = if let Some(max) = max_lines {
        body.lines().take(max).collect::<Vec<_>>().join("\n")
    } else {
        body.to_string()
    };

    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "file": file,
                    "title": title,
                    "body": text,
                }))
                .unwrap_or_default()
            );
        }
        "files" => {
            println!("{file}");
        }
        _ => {
            println!("{}", b(&format!("# {title}")));
            println!("{}", dim(&format!("── {file} ──")));
            println!();
            if line_numbers {
                for (i, line) in text.lines().enumerate() {
                    println!("{:>4}  {line}", i + 1);
                }
            } else {
                println!("{text}");
            }
        }
    }
}

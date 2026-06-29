use rqmd_core::SearchResult;

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
    std::env::var("NO_COLOR").is_err() && atty_stderr()
}

fn atty_stderr() -> bool {
    libc_isatty(1)
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
fn yellow(s: &str) -> String {
    if ansi_enabled() {
        format!("{YELLOW}{s}{RESET}")
    } else {
        s.to_string()
    }
}

// ── Format dispatch ───────────────────────────────────────────────────────────

pub fn print_results(results: &[SearchResult], format: &Format, show_full: bool) {
    match format {
        "json" => print_json(results),
        "csv" => print_csv(results),
        "md" | "markdown" => print_markdown(results, show_full),
        "xml" => print_xml(results),
        "files" => print_files(results),
        _ => print_cli(results, show_full),
    }
}

// ── CLI (colored terminal) ────────────────────────────────────────────────────

fn print_cli(results: &[SearchResult], show_full: bool) {
    if results.is_empty() {
        eprintln!("{}", dim("No results found."));
        return;
    }
    for (i, r) in results.iter().enumerate() {
        let score_label = yellow(&format!("{:.3}", r.score));
        let path_label = cyan(&format!("rrrqmd://{}/{}", r.collection, r.path));
        println!(
            "{} {} {}",
            dim(&format!("[{}]", i + 1)),
            b(&r.title),
            dim(&format!("#{}", r.docid))
        );
        println!("  {} {} {}", path_label, dim("·"), score_label);
        let snippet = if show_full {
            r.body.trim().to_string()
        } else {
            let chunk = r.best_chunk.trim();
            let lines: Vec<&str> = chunk.lines().take(4).collect();
            lines.join("\n")
        };
        if !snippet.is_empty() {
            for line in snippet.lines().take(if show_full { usize::MAX } else { 4 }) {
                println!("  {}", dim(line));
            }
        }
        if i < results.len() - 1 {
            println!();
        }
    }
    println!("\n{}", dim(&format!("{} result(s)", results.len())));
}

// ── JSON ──────────────────────────────────────────────────────────────────────

fn print_json(results: &[SearchResult]) {
    let arr: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "docid": format!("#{}", r.docid),
                "score": r.score,
                "file": r.file,
                "title": r.title,
                "collection": r.collection,
                "path": r.path,
                "body": r.body,
                "best_chunk": r.best_chunk,
                "best_chunk_pos": r.best_chunk_pos,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
}

// ── CSV ───────────────────────────────────────────────────────────────────────

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn print_csv(results: &[SearchResult]) {
    println!("docid,score,file,title,collection,path,snippet");
    for r in results {
        let snippet = r.best_chunk.lines().take(2).collect::<Vec<_>>().join(" ");
        println!(
            "{},{:.4},{},{},{},{},{}",
            csv_field(&format!("#{}", r.docid)),
            r.score,
            csv_field(&r.file),
            csv_field(&r.title),
            csv_field(&r.collection),
            csv_field(&r.path),
            csv_field(&snippet),
        );
    }
}

// ── Markdown ──────────────────────────────────────────────────────────────────

fn print_markdown(results: &[SearchResult], show_full: bool) {
    for (i, r) in results.iter().enumerate() {
        println!("## {}. {} `#{}`\n", i + 1, r.title, r.docid);
        println!("**File:** `{}` · **Score:** {:.3}\n", r.file, r.score);
        let body = if show_full { &r.body } else { &r.best_chunk };
        println!("{}\n", body.trim());
        println!("---\n");
    }
}

// ── XML ───────────────────────────────────────────────────────────────────────

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn print_xml(results: &[SearchResult]) {
    println!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    println!("<results count=\"{}\">", results.len());
    for r in results {
        println!("  <result>");
        println!("    <docid>#{}</docid>", xml_escape(&r.docid));
        println!("    <score>{:.4}</score>", r.score);
        println!("    <file>{}</file>", xml_escape(&r.file));
        println!("    <title>{}</title>", xml_escape(&r.title));
        println!("    <collection>{}</collection>", xml_escape(&r.collection));
        println!("    <path>{}</path>", xml_escape(&r.path));
        println!("    <snippet><![CDATA[{}]]></snippet>", r.best_chunk.trim());
        println!("  </result>");
    }
    println!("</results>");
}

// ── Files ─────────────────────────────────────────────────────────────────────

fn print_files(results: &[SearchResult]) {
    for r in results {
        println!("{}", r.file);
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

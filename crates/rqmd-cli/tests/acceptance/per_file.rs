//! `--per-file N` on `search`, `vsearch` and `query`: up to N matching
//! sections of one file, clamped to 5, one by default.
use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use rqmd_core::chunking::chunk_document;
use serde_json::Value;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, refute_vacuous_output, rqmd};

/// About `chunks` chunks of plain lines with the search term placed in the
/// middle of each chunk listed in `needle_chunks`, clear of the overlap it
/// shares with its neighbours. `tag` makes the body unique to
/// its document.
pub(super) fn sectioned_note(tag: &str, chunks: usize, needle_chunks: &[usize]) -> String {
    let mut text: String = (0..)
        .map(|i| format!("Sentence {i} of {tag} says nothing in particular.\n"))
        .scan(0, |len, line| {
            *len += line.len();
            (*len < 3100 * chunks + 2000).then_some(line)
        })
        .collect();
    let plan = chunk_document(&text);
    assert!(plan.len() >= chunks, "filler must span {chunks} chunks");
    let mut marks: Vec<(usize, &str)> = needle_chunks.iter().map(|&n| (n, "needleword")).collect();
    marks.sort_by_key(|(chunk, _)| std::cmp::Reverse(*chunk));
    for (chunk, sentence) in marks {
        let from = if chunk == 0 {
            0
        } else {
            plan[chunk - 1].pos + plan[chunk - 1].text.len()
        };
        let to = plan.get(chunk + 1).map_or(text.len(), |c| c.pos);
        let at = text[(from + to) / 2..]
            .find('\n')
            .map_or(text.len(), |n| (from + to) / 2 + n + 1);
        text.insert_str(at, &format!("{sentence}\n"));
    }
    text
}

fn indexed(index_dir: &Path, body: &str) -> Fixture {
    let fx = fixture_collection("notes", body);
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "notes"])
        .assert()
        .success();
    rqmd(index_dir).arg("update").assert().success();
    fx
}

fn search_json(index_dir: &Path, extra: &[&str]) -> Vec<Value> {
    let out = rqmd(index_dir)
        .args(["search", "needleword", "-n", "50", "--format", "json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&out);
    serde_json::from_slice(&out.stdout).expect("json results")
}

/// Without the flag a file is one result, as before.
#[test]
fn ac_1_default_search_returns_one_result_per_file() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed(index_dir.path(), &sectioned_note("a", 7, &[1, 3, 5]));

    assert_eq!(search_json(index_dir.path(), &[]).len(), 1);
    assert_eq!(search_json(index_dir.path(), &["--per-file", "1"]).len(), 1);
    let text = rqmd(index_dir.path())
        .args(["search", "needleword"])
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&text);
    assert!(!String::from_utf8_lossy(&text.stdout).contains("section"));
}

/// `--per-file 3` prints three different sections of the file, each with the
/// term in its snippet.
#[test]
fn ac_2_per_file_prints_each_matching_section_of_a_file() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed(index_dir.path(), &sectioned_note("a", 7, &[1, 3, 5]));

    let hits = search_json(index_dir.path(), &["--per-file", "3"]);

    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h["file"] == hits[0]["file"]));
    let mut lines: Vec<i64> = hits.iter().map(|h| h["line"].as_i64().unwrap()).collect();
    lines.sort_unstable();
    lines.dedup();
    assert_eq!(lines.len(), 3, "each section starts on a different line");
    assert!(
        hits.iter()
            .all(|h| h["snippet"].as_str().unwrap().contains("needleword"))
    );
}

/// A value above 5 is capped at 5 and 0 counts as 1.
#[test]
fn ac_3_per_file_is_capped_at_five() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed(
        index_dir.path(),
        &sectioned_note("a", 9, &[0, 1, 2, 3, 4, 5, 6, 7, 8]),
    );

    assert_eq!(
        search_json(index_dir.path(), &["--per-file", "99"]).len(),
        5
    );
    assert_eq!(search_json(index_dir.path(), &["--per-file", "5"]).len(), 5);
    assert_eq!(search_json(index_dir.path(), &["--per-file", "0"]).len(), 1);
}

/// Sections come back by score, then by path; the sections of one file are
/// never split by another file with an equal score.
#[test]
fn ac_4_sections_are_ordered_by_score_then_path() {
    let index_dir = TempDir::new().expect("tempdir");
    // Same shape and length, so both files score the same.
    let fx = fixture_collection("notes", &sectioned_note("a", 7, &[1, 3]));
    std::fs::rename(fx.path.join("doc.md"), fx.path.join("a.md")).unwrap();
    std::fs::write(fx.path.join("b.md"), sectioned_note("b", 7, &[1, 3])).unwrap();
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "notes"])
        .assert()
        .success();
    rqmd(index_dir.path()).arg("update").assert().success();

    let hits = search_json(index_dir.path(), &["--per-file", "2"]);

    let files: Vec<&str> = hits.iter().map(|h| h["file"].as_str().unwrap()).collect();
    assert_eq!(
        files,
        [
            "rqmd://notes/a.md",
            "rqmd://notes/a.md",
            "rqmd://notes/b.md",
            "rqmd://notes/b.md"
        ]
    );
    let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|w| w[0] >= w[1]), "{scores:?}");
}

/// A file that appears more than once is labelled in words, without colour, so
/// the sections can be told apart under `NO_COLOR` and `TERM=dumb`.
#[test]
fn ac_5_repeated_sections_are_labelled_in_plain_text() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed(index_dir.path(), &sectioned_note("a", 7, &[1, 3]));

    for (format, first, second) in [
        ("cli", "section 1 of 2", "section 2 of 2"),
        ("md", "**section:** 1 of 2", "**section:** 2 of 2"),
        ("xml", "section=\"1 of 2\"", "section=\"2 of 2\""),
    ] {
        rqmd(index_dir.path())
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .args([
                "search",
                "needleword",
                "--per-file",
                "2",
                "--format",
                format,
            ])
            .assert()
            .success()
            .stdout(contains(first))
            .stdout(contains(second))
            .stdout(predicates::str::contains('\x1b').not());
    }
}

/// All three search commands take the flag.
#[test]
fn ac_6_the_flag_is_listed_for_query_search_and_vsearch() {
    let index_dir = TempDir::new().expect("tempdir");
    for command in ["query", "search", "vsearch"] {
        rqmd(index_dir.path())
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(contains("--per-file"));
    }
}

/// `--format files` lists a file once however many of its sections match.
#[test]
fn ac_7_files_format_lists_each_file_once() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed(index_dir.path(), &sectioned_note("a", 7, &[1, 3, 5]));

    let out = rqmd(index_dir.path())
        .args([
            "search",
            "needleword",
            "--per-file",
            "3",
            "--format",
            "files",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    refute_vacuous_output(&out);
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 1);
}

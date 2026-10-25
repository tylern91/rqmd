//! `--recency <half-life>` on `query`, `search` and `vsearch`: opt-in weighting
//! by file age. The `query` and `vsearch` ranking needs a model, so it is
//! covered by the stub-backend tests in `rqmd-core`; this tier pins the flag
//! contract and runs `search` end to end.

use std::path::Path;
use std::time::{Duration, SystemTime};

use predicates::str::contains;
use serde_json::Value;
use tempfile::TempDir;

use super::helpers::{refute_vacuous_output, rqmd};
use super::per_file::sectioned_note;

const DAY: u64 = 86_400;

/// Index the files of `docs`, `(name, body, age in days)`, as collection
/// `notes` under `index_dir`, each last modified that long ago. The returned
/// directory holds the files and must outlive the searches.
fn aged_collection(index_dir: &Path, docs: &[(&str, &str, u64)]) -> TempDir {
    let root = TempDir::new().expect("tempdir");
    for (name, body, age_days) in docs {
        let path = root.path().join(name);
        std::fs::write(&path, body).expect("write doc");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open doc")
            .set_modified(SystemTime::now() - Duration::from_secs(age_days * DAY))
            .expect("set mtime");
    }
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(root.path())
        .args(["--name", "notes"])
        .assert()
        .success();
    rqmd(index_dir).arg("update").assert().success();
    root
}

/// The files `search` returned, in order.
fn search_files(index_dir: &Path, term: &str, extra: &[&str]) -> Vec<String> {
    let out = rqmd(index_dir)
        .args(["search", term, "-n", "50", "--format", "json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&out);
    let hits: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json results");
    hits.iter()
        .map(|h| h["file"].as_str().expect("file").to_string())
        .collect()
}

/// The flag is listed in `query --help` with its units and the floor.
#[test]
fn ac_1_help_documents_the_flag_its_units_and_the_floor() {
    let index_dir = TempDir::new().expect("tempdir");

    rqmd(index_dir.path())
        .args(["query", "--help"])
        .assert()
        .success()
        .stdout(contains("--recency <HALF_LIFE>"))
        .stdout(contains("30d"))
        .stdout(contains("50%"));
}

/// A half-life that is zero, negative, fractional, without a unit, in an
/// unknown unit or over ten years is a usage error that shows what to type.
#[test]
fn ac_2_a_bad_half_life_is_a_usage_error_with_an_example() {
    let index_dir = TempDir::new().expect("tempdir");

    for bad in ["0d", "-5d", "1.5d", "30", "30m", "3651d"] {
        rqmd(index_dir.path())
            .args(["query", "anything"])
            .arg(format!("--recency={bad}"))
            .assert()
            .failure()
            .code(2)
            .stderr(contains("invalid half-life"))
            .stderr(contains("30d, 2w or 12h"));
    }
}

/// `search` and `vsearch` list the flag with its units and the floor, as
/// `query` does.
#[test]
fn ac_3_search_and_vsearch_help_document_the_flag() {
    let index_dir = TempDir::new().expect("tempdir");

    for command in ["search", "vsearch"] {
        rqmd(index_dir.path())
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(contains("--recency <HALF_LIFE>"))
            .stdout(contains("30d"))
            .stdout(contains("50%"));
    }
}

/// A bad half-life on `search` or `vsearch` is the same usage error, before
/// the index is opened or a model loaded.
#[test]
fn ac_4_search_and_vsearch_refuse_a_bad_half_life_with_an_example() {
    let index_dir = TempDir::new().expect("tempdir");

    for command in ["search", "vsearch"] {
        for bad in ["0d", "-5d", "1.5d", "30", "30m", "3651d"] {
            rqmd(index_dir.path())
                .args([command, "anything"])
                .arg(format!("--recency={bad}"))
                .assert()
                .failure()
                .code(2)
                .stderr(contains("invalid half-life"))
                .stderr(contains("30d, 2w or 12h"));
        }
    }
}

/// Of two equally relevant files `search --recency 30d` lists the one
/// modified later first, whichever it is, and says on stderr that the order
/// is weighted.
#[test]
fn ac_5_search_with_recency_lists_the_newer_of_two_equal_files_first() {
    for (a_days, b_days, first) in [(400, 2, "b.md"), (2, 400, "a.md")] {
        let index_dir = TempDir::new().expect("tempdir");
        let _root = aged_collection(
            index_dir.path(),
            &[
                ("a.md", "alphamark note one\n", a_days),
                ("b.md", "alphamark note two\n", b_days),
            ],
        );

        let files = search_files(index_dir.path(), "alphamark", &["--recency", "30d"]);

        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0],
            format!("rqmd://notes/{first}"),
            "a {a_days}d, b {b_days}d"
        );
        rqmd(index_dir.path())
            .args(["search", "alphamark", "--recency", "30d"])
            .assert()
            .success()
            .stderr(contains("rqmd: results are weighted by file age"))
            .stderr(contains("half-life 30d"));
    }
}

/// A strongly relevant old file still appears with the flag, and the recent
/// file just outside the plain `-n` cut is found.
#[test]
fn ac_6_search_with_recency_keeps_old_hits_and_finds_a_recent_one_past_the_cut() {
    let index_dir = TempDir::new().expect("tempdir");
    let mut docs: Vec<(String, String, u64)> = (0..5)
        .map(|i| {
            (
                format!("old{i}.md"),
                "alphamark alphamark alphamark note\n".to_string(),
                400,
            )
        })
        .collect();
    docs.push(("z-recent.md".to_string(), "alphamark note\n".to_string(), 1));
    let docs: Vec<(&str, &str, u64)> = docs
        .iter()
        .map(|(n, b, a)| (n.as_str(), b.as_str(), *a))
        .collect();
    let _root = aged_collection(index_dir.path(), &docs);
    let top = |extra: &[&str]| {
        let out = rqmd(index_dir.path())
            .args(["search", "alphamark", "-n", "2", "--format", "json"])
            .args(extra)
            .assert()
            .success()
            .get_output()
            .clone();
        let hits: Vec<Value> = serde_json::from_slice(&out.stdout).expect("json results");
        hits.iter()
            .map(|h| h["file"].as_str().expect("file").to_string())
            .collect::<Vec<_>>()
    };

    let plain = top(&[]);
    let weighted = top(&["--recency", "30d"]);

    assert_eq!(plain.len(), 2);
    assert!(
        !plain.contains(&"rqmd://notes/z-recent.md".to_string()),
        "{plain:?}"
    );
    assert_eq!(weighted.len(), 2);
    assert!(
        weighted.contains(&"rqmd://notes/z-recent.md".to_string()),
        "{weighted:?}"
    );
    // The old files still match: without a recent rival they are all there.
    let all = search_files(index_dir.path(), "alphamark", &["--recency", "30d"]);
    assert_eq!(all.len(), 6);
}

/// Without the flag `search` prints what it printed before, byte for byte,
/// and says nothing about age on stderr.
#[test]
fn ac_7_search_without_recency_output_is_unchanged() {
    let index_dir = TempDir::new().expect("tempdir");
    let _root = aged_collection(
        index_dir.path(),
        &[
            ("a.md", "alphamark note one\n", 900),
            ("b.md", "alphamark alphamark note two\n", 1),
        ],
    );
    let golden = "rqmd://notes/b.md:1 #363794\nTitle: b\nScore:  19%\n\n\
        @@ -1,1 @@ (0 before, 0 after)\nalphamark alphamark note two\n\n\n\
        rqmd://notes/a.md:1 #60f09f\nTitle: a\nScore:  16%\n\n\
        @@ -1,1 @@ (0 before, 0 after)\nalphamark note one\n";

    let out = rqmd(index_dir.path())
        .args(["search", "alphamark"])
        .assert()
        .success()
        .get_output()
        .clone();

    assert_eq!(String::from_utf8_lossy(&out.stdout), golden);
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `--per-file` and `--recency` together: the newer file's sections come
/// first, whichever file it is, and every file keeps its sections.
#[test]
fn ac_8_search_per_file_sections_work_with_recency() {
    for (a_days, b_days, first, second) in [(2, 400, "a.md", "b.md"), (400, 2, "b.md", "a.md")] {
        let index_dir = TempDir::new().expect("tempdir");
        let _root = aged_collection(
            index_dir.path(),
            &[
                ("a.md", &sectioned_note("one", 5, &[1, 3]), a_days),
                ("b.md", &sectioned_note("two", 5, &[1, 3]), b_days),
            ],
        );

        let files = search_files(
            index_dir.path(),
            "needleword",
            &["--per-file", "2", "--recency", "30d"],
        );

        let file = |name: &str| format!("rqmd://notes/{name}");
        assert_eq!(
            files,
            [file(first), file(first), file(second), file(second)],
            "a {a_days}d, b {b_days}d"
        );
    }
}

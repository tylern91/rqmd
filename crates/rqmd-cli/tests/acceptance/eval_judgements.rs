//! `rqmd eval --judgements`: relevance evaluation against a user's own index
//! (github.com/tylern91/rqmd#153).
//!
//! The AC numbers follow the issue's Acceptance Criteria checklist in order.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use predicates::str::contains;
use serde_json::Value;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, refute_vacuous_output, rqmd};

const GOLDEN_BM25: &str = include_str!("golden/eval_builtin_bm25.stdout");
const GOLDEN_BM25_VERBOSE: &str = include_str!("golden/eval_builtin_bm25_verbose.stdout");

/// Collection `notes` with four documents, indexed for keyword search only.
///
/// `ownership` matches `alpha.md` (five times) and `gamma.md` (once), so BM25
/// ranks alpha first; `garbage` matches only `beta.md`; `delta.md` matches
/// neither query. Nothing contains `quantum`.
fn indexed_notes(index_dir: &Path) -> Fixture {
    let fx = fixture_collection("notes", "placeholder\n");
    fs::remove_file(fx.path.join("doc.md")).expect("remove placeholder");
    let docs = [
        (
            "alpha.md",
            "# Alpha\nownership ownership ownership ownership ownership\n",
        ),
        (
            "gamma.md",
            "# Gamma\nthis longer note mentions ownership once among many other unrelated \
             words about cargo and tooling and builds and releases\n",
        ),
        ("delta.md", "# Delta\nlifetimes and borrowing\n"),
        (
            "beta.md",
            "# Beta\npython garbage collector reference counting\n",
        ),
    ];
    for (name, body) in docs {
        fs::write(fx.path.join(name), body).expect("write doc");
    }
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "notes"])
        .assert()
        .success();
    rqmd(index_dir)
        .arg("update")
        .assert()
        .success()
        .stdout(contains("Indexed: 0 new, 0 updated, 4 unchanged"));
    fx
}

/// Three queries over `indexed_notes`; see the hand computation in
/// `ac_3_reports_every_metric_for_bm25_with_json`.
const JUDGEMENTS: &str = r#"{"queries": [
  {"query": "ownership", "relevant": {"notes/alpha.md": 1, "notes/gamma.md": 3, "notes/delta.md": 2}},
  {"query": "garbage", "relevant": ["notes/beta.md"]},
  {"query": "quantum", "relevant": ["notes/beta.md"]}
]}"#;

fn write_file(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write file");
    path
}

/// Every file under `dir` with its size and a content hash, so a test can
/// tell that nothing was written, created or removed.
fn snapshot(dir: &Path) -> BTreeMap<String, (u64, u64)> {
    use std::hash::{DefaultHasher, Hasher};
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (u64, u64)>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let bytes = fs::read(&path).expect("read file");
                let mut hasher = DefaultHasher::new();
                hasher.write(&bytes);
                let rel = path
                    .strip_prefix(root)
                    .expect("prefix")
                    .display()
                    .to_string();
                out.insert(rel, (bytes.len() as u64, hasher.finish()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn run_json(index_dir: &Path, judgements: &Path, extra: &[&str]) -> Value {
    let out = rqmd(index_dir)
        .args(["eval", "--judgements"])
        .arg(judgements)
        .arg("--json")
        .args(extra)
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&out);
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON document")
}

fn close(value: &Value, expected: f64) {
    let actual = value.as_f64().expect("number");
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

/// `rqmd eval` with no new argument prints the same bytes and exits the same
/// way as before this feature: the golden files were captured from the
/// v0.33.0 build before `--judgements` existed. The new options need
/// `--judgements`, and the run creates nothing in the index directory.
#[test]
fn ac_1_bare_eval_is_unchanged() {
    let index_dir = TempDir::new().expect("tempdir");

    let bare = rqmd(index_dir.path())
        .arg("eval")
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&bare);
    assert_eq!(String::from_utf8_lossy(&bare.stdout), GOLDEN_BM25);
    assert!(
        bare.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&bare.stderr)
    );

    let explicit = rqmd(index_dir.path())
        .args(["eval", "--mode", "bm25"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert_eq!(String::from_utf8_lossy(&explicit.stdout), GOLDEN_BM25);

    let verbose = rqmd(index_dir.path())
        .args(["eval", "--mode", "bm25", "--verbose"])
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&verbose);
    assert_eq!(
        String::from_utf8_lossy(&verbose.stdout),
        GOLDEN_BM25_VERBOSE
    );

    // The new options mean nothing without a judgement file.
    for flag in ["--json", "--k=3", "--no-rerank", "--no-expand"] {
        rqmd(index_dir.path())
            .args(["eval", flag])
            .assert()
            .failure()
            .stderr(contains("--judgements"));
    }

    assert_eq!(
        fs::read_dir(index_dir.path()).expect("read_dir").count(),
        0,
        "the built-in gate must not create an index"
    );
}

/// A judgement file scores an existing index without changing it, and a
/// missing index is an error that creates nothing.
#[test]
fn ac_2_judgement_file_leaves_the_index_untouched() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_notes(index_dir.path());
    let files = TempDir::new().expect("tempdir");
    let judgements = write_file(files.path(), "j.json", JUDGEMENTS);

    let before = snapshot(index_dir.path());
    assert!(before.keys().any(|k| k == "index.sqlite"), "{before:?}");

    let out = rqmd(index_dir.path())
        .args(["eval", "--judgements"])
        .arg(&judgements)
        .assert()
        .success()
        .get_output()
        .clone();
    refute_vacuous_output(&out);
    run_json(index_dir.path(), &judgements, &[]);

    assert_eq!(snapshot(index_dir.path()), before, "the index changed");

    let missing_dir = files.path().join("no-index-here");
    rqmd(&missing_dir)
        .args(["eval", "--judgements"])
        .arg(&judgements)
        .assert()
        .failure()
        .stderr(contains("no index at"));
    assert!(!missing_dir.exists(), "evaluating must not create an index");
}

/// nDCG@10, MRR@10, Recall@K and Hit@K come out as JSON (and as text) for
/// bm25, and vec and hybrid refuse an index without embeddings instead of
/// scoring empty results as misses.
///
/// Hand computation over `indexed_notes` (grades alpha 1, gamma 3, delta 2):
///
/// - `ownership` ranks [alpha, gamma]. DCG = 1/log2(2) + 3/log2(3) = 2.892789;
///   ideal order [gamma 3, delta 2, alpha 1]: 3 + 2/log2(3) + 1/log2(4) =
///   4.761860. nDCG = 0.607492. RR = 1. Recall@10 = 2/3 (delta is not
///   retrieved). Recall@1 = 1/3. Hit = true.
/// - `garbage` ranks [beta]: nDCG 1, RR 1, Recall 1, Hit true.
/// - `quantum` matches nothing: every metric 0, Hit false.
///
/// Means: nDCG@10 = (0.607492 + 1 + 0) / 3, MRR@10 = 2/3,
/// Recall@10 = (2/3 + 1 + 0) / 3 = 5/9, Hit@10 = 2/3.
/// With `--k 1`: Recall@1 = (1/3 + 1 + 0) / 3 = 4/9, Hit@1 = 2/3.
#[test]
fn ac_3_reports_every_metric_for_bm25_with_json() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_notes(index_dir.path());
    let files = TempDir::new().expect("tempdir");
    let judgements = write_file(files.path(), "j.json", JUDGEMENTS);

    let report = run_json(index_dir.path(), &judgements, &[]);
    assert_eq!(report["mode"], "bm25");
    assert_eq!(report["k"], 10);
    assert_eq!(report["queries_scored"], 3);
    close(
        &report["metrics"]["ndcg_at_10"],
        (0.607_491_518_045_652_5 + 1.0) / 3.0,
    );
    close(&report["metrics"]["mrr_at_10"], 2.0 / 3.0);
    close(&report["metrics"]["recall_at_k"], 5.0 / 9.0);
    close(&report["metrics"]["hit_at_k"], 2.0 / 3.0);
    for key in ["p50", "p95", "max", "mean"] {
        assert!(report["latency_ms"][key].is_number(), "latency {key}");
    }
    assert_eq!(report["per_query"].as_array().expect("array").len(), 3);
    assert_eq!(report["per_query"][0]["ranked"][0], "notes/alpha.md");

    let at_one = run_json(index_dir.path(), &judgements, &["--k", "1"]);
    assert_eq!(at_one["k"], 1);
    close(&at_one["metrics"]["recall_at_k"], 4.0 / 9.0);
    close(&at_one["metrics"]["hit_at_k"], 2.0 / 3.0);
    // nDCG and MRR stay at 10 whatever K is.
    close(
        &at_one["metrics"]["ndcg_at_10"],
        (0.607_491_518_045_652_5 + 1.0) / 3.0,
    );

    let text = rqmd(index_dir.path())
        .args(["eval", "--judgements"])
        .arg(&judgements)
        .assert()
        .success()
        .stdout(contains("nDCG@10"))
        .stdout(contains("MRR@10"))
        .stdout(contains("Recall@10"))
        .stdout(contains("Hit@10"))
        .stdout(contains("Latency ms"))
        .get_output()
        .clone();
    refute_vacuous_output(&text);

    // vec and hybrid need stored embeddings; this index has none.
    for mode in ["vec", "hybrid"] {
        rqmd(index_dir.path())
            .args(["eval", "--mode", mode, "--judgements"])
            .arg(&judgements)
            .assert()
            .failure()
            .stdout("")
            .stderr(contains(format!("mode {mode} needs embeddings")));
    }
    rqmd(index_dir.path())
        .args(["eval", "--mode", "nonsense", "--judgements"])
        .arg(&judgements)
        .assert()
        .failure()
        .stderr(contains("expected bm25, vec or hybrid"));
}

/// A malformed judgement file stops the run with a reason and exit 1. A judged
/// document missing from the index is listed by name, left out of the
/// scoring, and makes the run exit 2; a query with no judged document left is
/// reported as skipped, not as a miss.
#[test]
fn ac_4_malformed_and_missing_are_reported_not_scored_as_misses() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fx = indexed_notes(index_dir.path());
    let files = TempDir::new().expect("tempdir");

    let malformed = [
        ("not json", "not valid JSON"),
        (r#"{"queries": []}"#, "no queries"),
        (
            r#"{"queries": [{"query": "q", "relevant": ["no-slash"]}]}"#,
            "not of the form \"collection/path\"",
        ),
        (
            r#"{"queries": [{"query": "q", "relevant": {"notes/a.md": "high"}}]}"#,
            "non-negative integer",
        ),
        (
            r#"{"queries": [{"query": "q", "relevant": {"notes/a.md": 0}}]}"#,
            "at least one document with a grade of 1",
        ),
        (
            r#"{"queries": [{"query": "q", "relevant": ["notes/a.md"], "relevent": 1}]}"#,
            "unknown field \"relevent\"",
        ),
    ];
    for (i, (body, reason)) in malformed.iter().enumerate() {
        let path = write_file(files.path(), &format!("bad{i}.json"), body);
        rqmd(index_dir.path())
            .args(["eval", "--judgements"])
            .arg(&path)
            .assert()
            .failure()
            .code(1)
            .stdout("")
            .stderr(contains("malformed judgement file"))
            .stderr(contains(*reason));
    }
    let jsonl = write_file(
        files.path(),
        "bad.jsonl",
        "{\"query\": \"q\", \"relevant\": [\"notes/alpha.md\"]}\nnope\n",
    );
    rqmd(index_dir.path())
        .args(["eval", "--judgements"])
        .arg(&jsonl)
        .assert()
        .failure()
        .code(1)
        .stderr(contains("line 2"));

    let missing = write_file(
        files.path(),
        "missing.json",
        r#"{"queries": [
            {"query": "ownership", "relevant": {"notes/alpha.md": 3, "notes/ghost.md": 2}},
            {"query": "zzz", "relevant": ["notes/nothere.md"]}
        ]}"#,
    );
    let out = rqmd(index_dir.path())
        .args(["eval", "--judgements"])
        .arg(&missing)
        .assert()
        .failure()
        .code(2)
        .stdout(contains("notes/ghost.md"))
        .stdout(contains("notes/nothere.md"))
        .stdout(contains(
            "every relevant document is missing from the index",
        ))
        .stderr(contains("2 judged document(s) are missing from the index"))
        .get_output()
        .clone();
    refute_vacuous_output(&out);

    let output = rqmd(index_dir.path())
        .args(["eval", "--json", "--judgements"])
        .arg(&missing)
        .assert()
        .failure()
        .code(2)
        .get_output()
        .clone();
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON on stdout");
    assert_eq!(report["queries_total"], 2);
    assert_eq!(report["queries_scored"], 1);
    assert_eq!(
        report["queries_skipped"].as_array().expect("array").len(),
        1
    );
    let listed: Vec<&str> = report["missing_documents"]
        .as_array()
        .expect("array")
        .iter()
        .map(|m| m["document"].as_str().expect("string"))
        .collect();
    assert_eq!(listed, ["notes/ghost.md", "notes/nothere.md"]);
    // ghost.md is not a miss: the one scored query still has a perfect nDCG.
    close(&report["metrics"]["ndcg_at_10"], 1.0);

    let all_gone = write_file(
        files.path(),
        "all-gone.json",
        r#"{"queries": [{"query": "zzz", "relevant": ["notes/nothere.md"]}]}"#,
    );
    rqmd(index_dir.path())
        .args(["eval", "--judgements"])
        .arg(&all_gone)
        .assert()
        .failure()
        .code(1)
        .stderr(contains("no query could be scored"));
}

/// Each metric has a unit test in `rqmd-core` whose comment carries the hand
/// computation; `cargo test --workspace` runs them. This keeps the pairing
/// from being lost: a metric function without such a test fails here.
#[test]
fn ac_5_every_metric_has_a_hand_computed_unit_test() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../rqmd-core/src/eval_metrics.rs"),
    )
    .expect("read eval_metrics.rs");
    let (code, tests) = source.split_once("#[cfg(test)]").expect("a test module");

    let pairs = [
        ("pub fn ndcg_at_k", "fn ndcg_"),
        ("pub fn reciprocal_rank_at_k", "fn reciprocal_rank_"),
        ("pub fn recall_at_k", "fn recall_"),
        ("pub fn hit_at_k", "fn hit_"),
        ("pub fn percentile", "fn percentile_"),
        ("pub fn mean", "fn mean_"),
    ];
    for (function, unit_test) in pairs {
        assert!(code.contains(function), "{function} is gone");
        assert!(tests.contains(unit_test), "no unit test `{unit_test}...`");
    }
    assert!(
        tests.contains("Hand computation"),
        "the unit tests lost their hand computations"
    );
}

/// `docs/CLI.md` documents the options and the judgement-file format, and the
/// stale example harness is gone with nothing left pointing at it.
#[test]
fn ac_6_docs_describe_the_format_and_the_stale_harness_is_gone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cli = fs::read_to_string(root.join("docs/CLI.md")).expect("read docs/CLI.md");
    for needle in [
        "--judgements",
        "--json",
        "--k",
        "collection/path",
        "\"relevant\"",
        ".jsonl",
        "nDCG@10",
        "MRR@10",
        "Recall@K",
        "Hit@K",
    ] {
        assert!(
            cli.contains(needle),
            "docs/CLI.md does not mention {needle}"
        );
    }

    assert!(
        !root.join("crates/rqmd-core/examples/eval.rs").exists(),
        "the stale example harness is back"
    );
    for doc in ["README.md", "CONTRIBUTING.md", "docs/CLI.md"] {
        let text = fs::read_to_string(root.join(doc)).expect("read doc");
        assert!(
            !text.contains("--example eval"),
            "{doc} still points at the removed example"
        );
    }
}

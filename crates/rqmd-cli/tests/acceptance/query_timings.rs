//! `rqmd query --timings` (rqmd#152): a hidden diagnostic that prints per-stage
//! durations and counts to stderr after the results. Off by default, and when
//! on it changes neither the results nor the exit code. These tests embed a
//! small index, so they need the embedding model, as `index_rebuild` does.

use std::path::Path;

use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd};

const PREFIX: &str = "rqmd: timings: ";

/// Index two collections: `bulk`, thirty astronomy notes, and `small`, one
/// note on an unrelated subject. A query about astronomy scoped to `small` is
/// far from its scope, so the vector search has to widen.
fn two_collections(index_dir: &Path) -> [super::helpers::Fixture; 2] {
    let bulk = fixture_collection("bulk", "telescope galaxy nebula astronomy observation\n");
    for n in 0..30 {
        std::fs::write(
            bulk.path.join(format!("star-{n}.md")),
            format!("Star {n}: a telescope observes a galaxy, a nebula and orbit number {n}.\n"),
        )
        .expect("write bulk note");
    }
    let small = fixture_collection("small", "xylophone marimba percussion mallets\n");
    for (fixture, name) in [(&bulk, "bulk"), (&small, "small")] {
        rqmd(index_dir)
            .args(["collection", "add"])
            .arg(&fixture.path)
            .args(["--name", name])
            .assert()
            .success();
    }
    rqmd(index_dir).arg("update").assert().success();
    rqmd(index_dir).arg("embed").assert().success();
    [bulk, small]
}

/// `query` over `scope`, retrieval only so the run is quick and repeatable.
fn query(index_dir: &Path, text: &str, scope: &str, extra: &[&str]) -> std::process::Output {
    rqmd(index_dir)
        .args(["query", text, "-c", scope, "--no-rerank", "--no-expand"])
        .args(extra)
        .output()
        .expect("run query")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The timings lines of a stderr, and the rest of it.
fn split_timings(stderr: &str) -> (Vec<&str>, Vec<&str>) {
    stderr.lines().partition(|l| l.starts_with(PREFIX))
}

/// [AC-1] Stage timings and counts can be printed for a single query without
/// changing its results or its default output: same stdout, same exit code,
/// and a stderr that only gains `rqmd: timings:` lines.
#[test]
fn ac_1_timings_print_stage_counts_to_stderr_and_leave_results_unchanged() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fixtures = two_collections(index_dir.path());

    let plain = query(index_dir.path(), "telescope galaxy", "bulk", &[]);
    let timed = query(index_dir.path(), "telescope galaxy", "bulk", &["--timings"]);

    refute_vacuous_output(&plain);
    assert!(plain.status.success() && timed.status.success());
    assert_eq!(
        plain.stdout, timed.stdout,
        "--timings must not touch stdout"
    );

    let (stderr_plain, stderr_timed) = (text(&plain.stderr), text(&timed.stderr));
    let (lines, rest) = split_timings(&stderr_timed);
    assert_eq!(
        rest.join("\n"),
        stderr_plain.lines().collect::<Vec<_>>().join("\n"),
        "apart from timings lines, stderr must be unchanged"
    );
    for stage in [
        "total ",
        "bm25 ",
        "embed ",
        "ann pass 1 [query] k=",
        "per-hit lookups=",
        "fusion ",
        "resolve candidates ",
        "rerank not run",
    ] {
        assert!(
            lines.iter().any(|l| l.contains(stage)),
            "no `{stage}` in timings:\n{}",
            lines.join("\n")
        );
    }
}

/// [AC-1] By default nothing is printed, and the flag is hidden from `--help`.
#[test]
fn ac_2_default_query_prints_no_timings_and_help_hides_the_flag() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fixtures = two_collections(index_dir.path());

    let plain = query(index_dir.path(), "telescope galaxy", "bulk", &[]);
    refute_vacuous_output(&plain);
    assert!(plain.status.success());
    assert!(
        !text(&plain.stderr).contains("timings"),
        "{}",
        text(&plain.stderr)
    );
    assert!(!text(&plain.stdout).contains("timings"));

    let help = rqmd(index_dir.path())
        .args(["query", "--help"])
        .output()
        .expect("run help");
    assert!(help.status.success());
    assert!(!text(&help.stdout).contains("--timings"));
}

/// [AC-1] Scoped to a collection far from the query, the report shows the
/// widening: a second ANN pass with a larger `k`, ending with in-scope hits,
/// and one per-hit lookup for every vector fetched.
#[test]
fn ac_3_scoped_query_far_from_its_scope_reports_each_widening_pass() {
    let index_dir = TempDir::new().expect("tempdir");
    let _fixtures = two_collections(index_dir.path());

    let timed = query(
        index_dir.path(),
        "telescope galaxy nebula",
        "small",
        &["--timings"],
    );
    assert!(timed.status.success(), "{}", text(&timed.stderr));
    let stderr = text(&timed.stderr);
    let (lines, _) = split_timings(&stderr);

    let passes: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.contains("ann pass "))
        .collect();
    assert!(
        passes.len() >= 2,
        "expected widening:\n{}",
        lines.join("\n")
    );
    let field = |pass: &str, name: &str| -> usize {
        let rest = pass.split(name).nth(1).expect(name);
        rest.split_whitespace().next().unwrap().parse().unwrap()
    };
    // The first pass asks for the candidate pool; the next asks for more
    // (capped at the 32 vectors in the index).
    assert_eq!(field(passes[0], " k="), 20, "{}", passes[0]);
    assert!(field(passes[1], " k=") > 20, "{}", passes[1]);
    for pass in &passes {
        // Every vector fetched is resolved by one lookup.
        assert_eq!(
            field(pass, "vectors fetched="),
            field(pass, "per-hit lookups="),
            "{pass}"
        );
    }
    assert!(
        !passes.last().unwrap().contains("in scope=0"),
        "the last pass must have found the scoped note:\n{}",
        lines.join("\n")
    );
}

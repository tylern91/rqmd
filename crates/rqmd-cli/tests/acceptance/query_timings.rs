//! `rqmd query --timings` (rqmd#152): a hidden diagnostic that prints per-stage
//! durations and counts to stderr after the results. Off by default, and when
//! on it changes neither the results nor the exit code.
//!
//! A `query` that runs to the end loads an inference model, and the acceptance
//! tier runs without one, so this tier pins the flag contract only: it is
//! hidden, `query`-only, and changes nothing when the query cannot run. The
//! stage figures, and that they leave the results alone, are covered by the
//! stub-backend tests in `rqmd-core` (`timings_*`) and `rqmd-cli`
//! (`commands::query::tests::timings_*`); measurements on a real index are in
//! docs/research/2026-10-28-scoped-vector-search-timings.md.

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

/// [AC-1] The flag is hidden from `query --help` yet accepted by `query`, and
/// the other search commands do not take it: it is a `query` diagnostic.
#[test]
fn ac_1_flag_is_hidden_from_help_but_accepted_by_query_only() {
    let index_dir = TempDir::new().expect("tempdir");

    let help = rqmd(index_dir.path())
        .args(["query", "--help"])
        .assert()
        .success()
        .get_output()
        .clone();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("--timeout"), "help should list query flags");
    assert!(!help.contains("--timings"), "{help}");

    // Accepted: with no index the query stops for that reason (exit 1), not
    // because the flag is unknown (exit 2).
    rqmd(index_dir.path())
        .args(["query", "anything", "--timings"])
        .assert()
        .failure()
        .code(1)
        .stderr(contains("no index at"));

    for command in ["search", "vsearch"] {
        rqmd(index_dir.path())
            .args([command, "anything", "--timings"])
            .assert()
            .failure()
            .code(2)
            .stderr(contains("unexpected argument '--timings'"));
    }
}

/// [AC-1] When the query cannot run, `--timings` changes nothing: the same
/// stdout, stderr and exit code as without it, and no timings line.
#[test]
fn ac_2_flag_changes_nothing_when_the_query_cannot_run() {
    let index_dir = TempDir::new().expect("tempdir");
    let run = |extra: &[&str]| {
        rqmd(index_dir.path())
            .args(["query", "anything"])
            .args(extra)
            .output()
            .expect("run query")
    };

    let plain = run(&[]);
    let timed = run(&["--timings"]);

    assert_eq!(plain.status.code(), Some(1), "no index is exit 1");
    assert_eq!(plain.status.code(), timed.status.code());
    assert_eq!(plain.stdout, timed.stdout);
    assert_eq!(plain.stderr, timed.stderr);
    let stderr = String::from_utf8_lossy(&timed.stderr);
    assert!(stderr.contains("no index at"), "{stderr}");
    assert!(!stderr.contains("timings"), "{stderr}");
}

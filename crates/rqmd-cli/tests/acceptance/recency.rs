//! `rqmd query --recency <half-life>`: opt-in weighting by file age. The
//! ranking itself needs a model, so it is covered by the stub-backend tests in
//! `rqmd-core`; this tier pins the flag contract.

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

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

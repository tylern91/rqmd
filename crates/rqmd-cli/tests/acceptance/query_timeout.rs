//! `rqmd query --timeout`: a deadline that passes before reranking returns
//! the fused results by default; `--timeout-strict` restores exit 124. The
//! behaviour itself needs a model, so it is covered by the stub-backend unit
//! tests in `rqmd-core` and `rqmd-mcp`; this tier pins the flag contract.

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::rqmd;

/// `--timeout-strict` changes what `--timeout` does, so on its own it has no
/// meaning and is refused with a usage error that names the missing flag.
#[test]
fn ac_1_timeout_strict_requires_timeout() {
    let index_dir = TempDir::new().expect("tempdir");

    rqmd(index_dir.path())
        .args(["query", "anything", "--timeout-strict"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("required arguments were not provided"))
        .stderr(contains("--timeout <SECS>"));
}

/// The flag is listed in `query --help`, next to `--timeout`, and the help
/// says what each timeout mode does.
#[test]
fn ac_2_help_documents_the_soft_default_and_the_strict_flag() {
    let index_dir = TempDir::new().expect("tempdir");

    rqmd(index_dir.path())
        .args(["query", "--help"])
        .assert()
        .success()
        .stdout(contains("--timeout-strict"))
        .stdout(contains("fusion order"));
}

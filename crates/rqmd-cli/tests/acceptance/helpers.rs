use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use tempfile::TempDir;

/// Build an `rqmd` invocation scoped to `index_dir` via `--index-dir`.
pub fn rqmd(index_dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("rqmd").expect("rqmd binary");
    cmd.arg("--index-dir").arg(index_dir);
    cmd
}

/// Fails the test if `out`'s stdout is empty or whitespace-only — a test
/// cannot pass by asserting against nothing.
pub fn refute_vacuous_output(out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "expected non-empty stdout, got: {stdout:?} (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A scratch directory containing one markdown file, suitable for
/// `collection add`. `_root` must be kept alive for the collection's
/// lifetime — dropping it deletes the directory.
pub struct Fixture {
    pub _root: TempDir,
    pub path: PathBuf,
}

pub fn fixture_collection(name: &str, body: &str) -> Fixture {
    let root = TempDir::new().expect("tempdir");
    let path = root.path().join(name);
    std::fs::create_dir_all(&path).expect("mkdir collection dir");
    std::fs::write(path.join("doc.md"), body).expect("write fixture doc");
    Fixture { _root: root, path }
}

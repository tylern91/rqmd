//! Commands that only read an index never create one (#156). Against a
//! directory with no `index.sqlite` they leave the filesystem as they found
//! it and say where no index was found and how to create one. `search`,
//! `vsearch`, `query`, `get` and `multi-get` exit 1, because the data asked
//! for cannot exist; `status`, `doctor`, `collection list`, `context list` and
//! `ls` exit 0 with that message. Commands that write keep creating the index.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use predicates::str::contains;
use tempfile::TempDir;

use super::helpers::{fixture_collection, refute_vacuous_output, rqmd, rqmd_in_cwd};

/// Read commands that fail without an index: the caller asked for data.
const FAILING: &[&[&str]] = &[
    &["search", "foo"],
    &["vsearch", "foo"],
    &["query", "foo"],
    &["get", "foo.md"],
    &["multi-get", "foo.md"],
];

/// Read commands that report a missing index and succeed.
const INFORMATIONAL: &[&[&str]] = &[
    &["status"],
    &["doctor"],
    &["collection", "list"],
    &["context", "list"],
    &["ls"],
];

fn all_read_commands() -> impl Iterator<Item = &'static [&'static str]> {
    FAILING.iter().chain(INFORMATIONAL).copied()
}

fn run(index_dir: &Path, args: &[&str]) -> Output {
    rqmd(index_dir)
        .args(args)
        .timeout(Duration::from_secs(60))
        .output()
        .expect("run rqmd")
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Every read command, run against a directory that does not exist, leaves
/// that directory absent.
#[test]
fn ac_1_read_commands_leave_a_missing_index_dir_absent() {
    let parent = TempDir::new().expect("tempdir");
    for args in all_read_commands() {
        let index_dir = parent.path().join("idx");
        let out = run(&index_dir, args);
        assert!(
            !combined(&out).trim().is_empty(),
            "`rqmd {}` printed nothing",
            args.join(" ")
        );
        assert!(
            !index_dir.exists(),
            "`rqmd {}` created {}",
            args.join(" "),
            index_dir.display()
        );
    }
    assert_eq!(
        std::fs::read_dir(parent.path())
            .expect("read parent")
            .count(),
        0,
        "nothing may be created beside the index dir either"
    );
}

/// Each read command names the resolved index directory and the command that
/// creates an index.
#[test]
fn ac_2_read_commands_name_the_index_dir_and_how_to_create_one() {
    let parent = TempDir::new().expect("tempdir");
    let index_dir = parent.path().join("idx");
    let shown = index_dir.display().to_string();
    for args in all_read_commands() {
        let text = combined(&run(&index_dir, args));
        assert!(
            text.contains(&shown),
            "`rqmd {}` does not name {shown}: {text}",
            args.join(" ")
        );
        assert!(
            text.contains("rqmd collection add"),
            "`rqmd {}` does not say how to create an index: {text}",
            args.join(" ")
        );
    }
}

/// Exit status for a missing index: 1 for commands that ask for data, 0 with
/// the message on stdout for the informational ones.
#[test]
fn ac_3_missing_index_exit_status_is_as_documented() {
    let parent = TempDir::new().expect("tempdir");
    let index_dir = parent.path().join("idx");
    let message = format!("no index at {}", index_dir.display());
    for args in FAILING {
        rqmd(&index_dir)
            .args(*args)
            .timeout(Duration::from_secs(60))
            .assert()
            .code(1)
            .stderr(contains(format!("rqmd: {message}")));
    }
    for args in INFORMATIONAL {
        let out = rqmd(&index_dir)
            .args(*args)
            .timeout(Duration::from_secs(60))
            .assert()
            .success()
            .get_output()
            .clone();
        refute_vacuous_output(&out);
        let stdout = String::from_utf8_lossy(&out.stdout);
        if args[0] == "doctor" {
            assert!(
                stdout.contains("SQLite exists: NO — run `rqmd collection add <path>`"),
                "{stdout}"
            );
        } else {
            assert!(
                stdout.contains(&message),
                "`rqmd {}`: {stdout}",
                args.join(" ")
            );
        }
    }
}

/// `collection add`, `update`, `embed` and `init` still create a new index.
#[test]
fn ac_4_write_commands_still_create_the_index() {
    let parent = TempDir::new().expect("tempdir");
    let fx = fixture_collection("notes", "# Note\n\nbody\n");

    let added = parent.path().join("added");
    rqmd(&added)
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "notes"])
        .assert()
        .success();
    assert!(added.join("index.sqlite").is_file(), "collection add");

    // `update` and `embed` take the index lock before opening the store, and
    // the lock needs the directory to exist, so they are run, as before this
    // fix, on an existing directory that holds no index yet.
    let updated = parent.path().join("updated");
    std::fs::create_dir(&updated).expect("mkdir updated");
    rqmd(&updated).arg("update").assert().success();
    assert!(updated.join("index.sqlite").is_file(), "update");

    let embedded = parent.path().join("embedded");
    std::fs::create_dir(&embedded).expect("mkdir embedded");
    rqmd(&embedded)
        .arg("embed")
        .assert()
        .success()
        .stdout(contains("No collections to embed."));
    assert!(embedded.join("index.sqlite").is_file(), "embed");

    let project = TempDir::new().expect("tempdir");
    rqmd_in_cwd(project.path())
        .arg("init")
        .assert()
        .success()
        .stdout(contains("Initialized local index"));
    assert!(
        project.path().join(".rqmd").join("index.sqlite").is_file(),
        "init"
    );

    // And a read command then finds the index the write created.
    rqmd(&added)
        .args(["collection", "list"])
        .assert()
        .success()
        .stdout(contains("notes"));
}

/// Regression for the side effect itself: an index directory that exists but
/// holds no index stays empty after every read command and after `rqmd mcp`
/// starts and stops on it.
#[test]
fn ac_5_read_commands_and_mcp_leave_an_empty_index_dir_empty() {
    let index_dir = TempDir::new().expect("tempdir");
    let entries = || {
        std::fs::read_dir(index_dir.path())
            .expect("read index dir")
            .map(|e| e.expect("entry").file_name())
            .collect::<Vec<_>>()
    };
    for args in all_read_commands() {
        let out = run(index_dir.path(), args);
        assert!(
            combined(&out).contains("rqmd collection add"),
            "`rqmd {}`: {}",
            args.join(" "),
            combined(&out)
        );
        assert_eq!(
            entries(),
            Vec::<std::ffi::OsString>::new(),
            "`rqmd {}`",
            args.join(" ")
        );
    }

    // stdio MCP with stdin closed: the server starts, sees EOF and stops.
    rqmd(index_dir.path())
        .arg("mcp")
        .timeout(Duration::from_secs(60))
        .write_stdin("")
        .assert()
        .stderr(contains(format!(
            "rqmd: no index at {}",
            index_dir.path().display()
        )));
    assert_eq!(entries(), Vec::<std::ffi::OsString>::new(), "`rqmd mcp`");
}

//! Escape sequences in rqmd output: absent when piped, absent under `NO_COLOR`,
//! and absent (with newline-terminated progress) under `TERM=dumb`.
use std::path::Path;
use std::process::{Command, Stdio};

use predicates::prelude::*;
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd};

const ESC: char = '\x1b';

/// An index holding one collection that has been added but not yet updated, so
/// the raw-backfill warning is still pending on later reads.
fn added_index(fx_path: &Path, index_dir: &Path) {
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(fx_path)
        .args(["--name", "notes"])
        .assert()
        .success();
}

/// Run `rqmd` under a pseudo-terminal (`script`) so both streams are a TTY,
/// with `env_args` applied through `env`. Returns the captured terminal
/// output, or `None` when no pseudo-terminal could be set up.
fn on_terminal(index_dir: &Path, env_args: &[&str], rqmd_args: &[&str]) -> Option<String> {
    let bin = env!("CARGO_BIN_EXE_rqmd");
    let mut argv: Vec<String> = vec!["env".into()];
    argv.extend(env_args.iter().map(|a| a.to_string()));
    argv.extend([
        bin.to_string(),
        "--index-dir".into(),
        index_dir.display().to_string(),
    ]);
    argv.extend(rqmd_args.iter().map(|a| a.to_string()));

    let mut cmd = Command::new("script");
    if cfg!(target_os = "macos") {
        cmd.args(["-q", "/dev/null"]).args(&argv);
    } else {
        let quoted: Vec<String> = argv
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect();
        cmd.args(["-qec", &quoted.join(" "), "/dev/null"]);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A carriage return not followed by a line feed: a redraw in place. A
/// terminal turns each newline into `\r\n`, which is not one.
fn has_bare_carriage_return(s: &str) -> bool {
    let b = s.as_bytes();
    b.iter()
        .enumerate()
        .any(|(i, &c)| c == b'\r' && b.get(i + 1) != Some(&b'\n'))
}

/// `status` prints styled headings and `collection add` redraws a progress
/// line, so a terminal run of both exercises stdout and stderr escapes. The
/// first run, with colours allowed, proves the terminal setup emits escapes at
/// all; `None` means it could not, and the caller skips.
fn terminal_run(index_dir: &Path, fx_path: &Path, env_args: &[&str]) -> Option<String> {
    let control = on_terminal(
        index_dir,
        &["-u", "NO_COLOR", "TERM=xterm-256color"],
        &["status"],
    )?;
    if !control.contains(ESC) {
        eprintln!("skipped: no pseudo-terminal output with escapes available");
        return None;
    }
    let add = on_terminal(
        index_dir,
        env_args,
        &["collection", "add", fx_path.to_str().expect("utf-8 path")],
    )?;
    let status = on_terminal(index_dir, env_args, &["status"])?;
    Some(format!("{add}{status}"))
}

/// Piped output, on both streams, is free of escape sequences for every
/// command that styles its output, including the warnings on stderr.
#[test]
fn ac_1_piped_output_carries_no_escapes() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("notes", "# Alpha\n\nbeta gamma\n");
    added_index(&fx.path, index_dir.path());

    let search = rqmd(index_dir.path())
        .args(["search", "beta"])
        .assert()
        .success()
        .stderr(contains_text("warning: index predates"));
    let out = search.get_output();
    assert!(!String::from_utf8_lossy(&out.stderr).contains(ESC));

    for args in [
        &["status"][..],
        &["update"],
        &["doctor"],
        &["embed", "--cleanup"],
    ] {
        let out = rqmd(index_dir.path())
            .args(args)
            .output()
            .expect("run rqmd");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(!stdout.contains(ESC), "{args:?} stdout: {stdout:?}");
        assert!(!stderr.contains(ESC), "{args:?} stderr: {stderr:?}");
    }

    // An empty collection has nothing to embed, which answers without a model.
    let empty_index = TempDir::new().expect("tempdir");
    let empty = TempDir::new().expect("tempdir");
    added_index(empty.path(), empty_index.path());
    let out = rqmd(empty_index.path())
        .arg("embed")
        .output()
        .expect("run rqmd");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("All content hashes already have embeddings"));
    assert!(!stdout.contains(ESC), "embed stdout: {stdout:?}");
    assert!(!String::from_utf8_lossy(&out.stderr).contains(ESC));
}

/// `NO_COLOR` removes every escape sequence from terminal output.
#[test]
fn ac_2_no_color_on_a_terminal_carries_no_escapes() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("notes", "# Alpha\n\nbeta gamma\n");
    let Some(out) = terminal_run(
        index_dir.path(),
        &fx.path,
        &["NO_COLOR=1", "TERM=xterm-256color"],
    ) else {
        return;
    };
    assert!(out.contains("Indexed 1 document(s)"), "output: {out:?}");
    assert!(out.contains("RQMD Status"), "output: {out:?}");
    assert!(!out.contains(ESC), "output: {out:?}");
}

/// `TERM=dumb` removes every escape sequence and never redraws a line in
/// place: progress is printed as newline-terminated lines.
#[test]
fn ac_3_dumb_terminal_has_no_escapes_and_no_redraw() {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("notes", "# Alpha\n\nbeta gamma\n");
    let Some(out) = terminal_run(index_dir.path(), &fx.path, &["TERM=dumb"]) else {
        return;
    };
    assert!(out.contains("Indexing doc.md (1)"), "output: {out:?}");
    assert!(out.contains("RQMD Status"), "output: {out:?}");
    assert!(!out.contains(ESC), "output: {out:?}");
    assert!(!has_bare_carriage_return(&out), "output: {out:?}");
}

fn contains_text(needle: &'static str) -> impl Predicate<[u8]> {
    predicates::str::contains(needle).from_utf8()
}

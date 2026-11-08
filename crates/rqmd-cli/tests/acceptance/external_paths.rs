//! Stage 1 of tylern91/rqmd#227: on a project-local `.rqmd/` index, `rqmd update`,
//! `rqmd watch` and the MCP `refresh: true` pass name a collection whose
//! directory resolves outside the project, once on stderr, and still index it.
//! An explicit `--index-dir` and the global index print nothing new, and a path
//! that does not exist keeps only its `directory not found` warning. Design:
//! docs/research/2026-10-27-external-collection-paths.md.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::helpers::{fixture_collection, rqmd, rqmd_in_cwd};

/// The text every warning for an outside collection ends with.
const TAIL: &str =
    "is outside the project directory; it is still indexed, and a future release will skip it";

/// Upper bound for anything the child processes are expected to do on their own.
const LIMIT: Duration = Duration::from_secs(30);

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    done()
}

/// An aged mtime keeps the stat-skip from re-reading a file on every pass.
fn age(file: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(file)
        .and_then(|f| f.set_modified(old))
        .expect("age file");
}

/// A scratch parent holding an empty project directory (with its `.rqmd/`
/// index, created through `--index-dir`) and room for siblings of it.
struct Project {
    parent: TempDir,
}

impl Project {
    fn new() -> Self {
        let parent = TempDir::new().expect("tempdir");
        std::fs::create_dir(parent.path().join("project")).expect("mkdir project");
        Self { parent }
    }

    fn dir(&self) -> PathBuf {
        self.parent.path().join("project")
    }

    fn index(&self) -> PathBuf {
        self.dir().join(".rqmd")
    }

    /// `rqmd collection add` through `--index-dir`, which stores `path` resolved.
    fn add(&self, name: &str, path: &Path) {
        rqmd(&self.index())
            .current_dir(self.dir())
            .args(["collection", "add"])
            .arg(path)
            .args(["--name", name])
            .assert()
            .success();
    }

    /// Overwrite a stored column, as an index file written by someone else would.
    fn set(&self, name: &str, column: &str, value: &str) {
        let db = Connection::open(self.index().join("index.sqlite")).expect("open db");
        let changed = db
            .execute(
                &format!("UPDATE store_collections SET {column} = ?2 WHERE name = ?1"),
                [name, value],
            )
            .expect("update collection row");
        assert_eq!(changed, 1, "no collection named {name}");
    }

    /// `rqmd update` through `--index-dir` until the index is settled, so a
    /// later pass is a plain incremental one.
    fn settle(&self) {
        for _ in 0..2 {
            rqmd(&self.index())
                .current_dir(self.dir())
                .arg("update")
                .assert()
                .success();
        }
    }

    /// `rqmd update` with no `--index-dir`, from the project: the index is
    /// picked up implicitly and is project-local. Returns stderr.
    fn update(&self) -> String {
        let out = rqmd_in_cwd(&self.dir())
            .arg("update")
            .output()
            .expect("run update");
        assert!(out.status.success(), "update failed: {out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn finds(&self, term: &str, file: &str) -> bool {
        let out = rqmd_in_cwd(&self.dir())
            .args(["search", term])
            .output()
            .expect("run search");
        String::from_utf8_lossy(&out.stdout).contains(file)
    }
}

fn warnings(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|l| l.contains(TAIL)).collect()
}

/// An update names a collection outside the project once, without mentioning
/// any flag, and still indexes it.
#[test]
fn ac_1_update_warns_once_for_an_outside_collection_and_still_indexes_it() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword lives outside\n");
    project.add("ext", &outside.path);

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("collection 'ext' (")
            && found[0].contains(outside.path.to_str().unwrap()),
        "{}",
        found[0]
    );
    assert!(
        !found[0].contains("allow-external-paths") && !found[0].contains("--"),
        "the warning must not name a flag: {}",
        found[0]
    );
    assert!(project.finds("extword", "ext/doc.md"), "still indexed");
}

/// A collection inside the project draws no warning.
#[test]
fn ac_2_a_collection_inside_the_project_is_not_warned_about() {
    let project = Project::new();
    let notes = project.dir().join("notes");
    std::fs::create_dir(&notes).expect("mkdir notes");
    std::fs::write(notes.join("doc.md"), "insideword\n").expect("write doc");
    project.add("notes", &notes);

    let stderr = project.update();

    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");
    assert!(project.finds("insideword", "notes/doc.md"));
}

/// An explicit `--index-dir`, and the global index, are the caller's own choice.
#[test]
fn ac_3_an_explicit_or_global_index_prints_nothing_new() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword lives outside\n");
    project.add("ext", &outside.path);

    let out = rqmd(&project.index())
        .current_dir(project.dir())
        .arg("update")
        .output()
        .expect("run update");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");

    // The global index lives under the home directory, so point that at a scratch one.
    let home = TempDir::new().expect("tempdir");
    let cwd = TempDir::new().expect("tempdir");
    let run = |args: &[&str]| {
        let mut cmd = rqmd_in_cwd(cwd.path());
        cmd.env("HOME", home.path())
            .env_remove("XDG_CACHE_HOME")
            .env_remove("RQMD_INDEX_DIR")
            .args(args);
        cmd
    };
    run(&["collection", "add"])
        .arg(&outside.path)
        .args(["--name", "ext"])
        .assert()
        .success();
    let out = run(&["update"]).output().expect("run update");
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");
    let search = run(&["search", "extword"]).output().expect("run search");
    assert!(String::from_utf8_lossy(&search.stdout).contains("ext/doc.md"));
}

/// The name and path come from the index file: control characters are replaced.
#[test]
fn ac_4_control_characters_in_the_warning_are_replaced() {
    let project = Project::new();
    let outside = TempDir::new().expect("tempdir");
    let dir = outside.path().join("x\u{1b}[2Jy");
    std::fs::create_dir(&dir).expect("mkdir with a control character");
    std::fs::write(dir.join("doc.md"), "ctrlword\n").expect("write doc");
    project.add("ext", &dir);
    project.set("ext", "name", "a\u{7}b");

    let stderr = project.update();

    assert!(
        !stderr.contains('\u{1b}') && !stderr.contains('\u{7}'),
        "{stderr:?}"
    );
    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr:?}");
    assert!(found[0].contains("collection 'a?b' ("), "{}", found[0]);
    assert!(found[0].contains("x?[2Jy"), "{}", found[0]);
}

/// A relative stored path is read against the project directory first.
#[test]
fn ac_5_a_relative_stored_path_is_resolved_against_the_project() {
    let project = Project::new();
    let sibling = project.parent.path().join("sibling");
    std::fs::create_dir(&sibling).expect("mkdir sibling");
    std::fs::write(sibling.join("doc.md"), "siblingword\n").expect("write doc");
    let inner = project.dir().join("inner");
    std::fs::create_dir(&inner).expect("mkdir inner");
    std::fs::write(inner.join("doc.md"), "innerword\n").expect("write doc");
    project.add("out", &sibling);
    project.add("in", &inner);
    project.set("out", "path", "../sibling");
    project.set("in", "path", "inner");

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("collection 'out' (../sibling)"),
        "{}",
        found[0]
    );
    assert!(project.finds("siblingword", "out/doc.md"));
    assert!(project.finds("innerword", "in/doc.md"));
}

/// A symlinked root is judged by where it points, not where it sits.
#[test]
fn ac_6_a_symlinked_root_is_judged_by_its_target() {
    let project = Project::new();
    let outside = fixture_collection("ext", "linkword\n");
    let inside = project.dir().join("real");
    std::fs::create_dir(&inside).expect("mkdir real");
    std::fs::write(inside.join("doc.md"), "realword\n").expect("write doc");
    // A link in the project to a directory outside it, and a link outside to one inside.
    std::os::unix::fs::symlink(&outside.path, project.dir().join("out-link")).expect("symlink");
    let in_link = project.parent.path().join("in-link");
    std::os::unix::fs::symlink(&inside, &in_link).expect("symlink");
    project.add("out", &outside.path);
    project.add("in", &inside);
    project.set("out", "path", "out-link");
    project.set("in", "path", in_link.to_str().unwrap());

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("collection 'out' (out-link)"),
        "{}",
        found[0]
    );
}

/// A path that does not exist keeps today's warning and gains no new one.
#[test]
fn ac_7_a_nonexistent_path_gets_only_the_directory_not_found_warning() {
    let project = Project::new();
    let outside = fixture_collection("ext", "gone\n");
    project.add("ext", &outside.path);
    let missing = project.parent.path().join("missing");
    project.set("ext", "path", missing.to_str().unwrap());

    let stderr = project.update();

    assert!(stderr.contains("directory not found"), "stderr: {stderr}");
    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");
}

/// A running `rqmd watch` started from the project, its stderr kept in a file.
struct Watch {
    child: Child,
    stderr_path: PathBuf,
}

impl Watch {
    fn start(project: &Project) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let stderr_path = project.parent.path().join("watch.stderr");
        let child = Command::new(program.get_program())
            .current_dir(project.dir())
            .args(["watch", "--interval", "1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).expect("stderr file"))
            .spawn()
            .expect("spawn rqmd watch");
        Self { child, stderr_path }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Watch passes over the same collection on every changed tick, and names an
/// outside one on the first pass only.
#[test]
fn ac_8_watch_warns_once_across_ticks() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    project.settle();

    let watch = Watch::start(&project);
    assert!(
        wait_until(LIMIT, || watch.stderr().contains(TAIL)),
        "no warning; stderr: {}",
        watch.stderr()
    );
    // Two more changes, each indexed by a later pass.
    for (file, word) in [("two.md", "secondword"), ("three.md", "thirdword")] {
        std::fs::write(outside.path.join(file), format!("{word}\n")).expect("write doc");
        assert!(
            wait_until(LIMIT, || project.finds(word, file)),
            "{file} never became searchable; stderr: {}",
            watch.stderr()
        );
    }

    let stderr = watch.stderr();
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(!stderr.contains("allow-external-paths"));
}

/// A running `rqmd mcp` (stdio) from the project that has finished the handshake.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    stderr_path: PathBuf,
}

impl Mcp {
    fn start(project: &Project) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let stderr_path = project.parent.path().join("mcp.stderr");
        let mut child = Command::new(program.get_program())
            .current_dir(project.dir())
            .args(["mcp", "--allow-refresh"])
            .env_remove("RQMD_MCP_HOST")
            .env_remove("RQMD_MCP_PORT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&stderr_path).expect("stderr file"))
            .spawn()
            .expect("spawn rqmd mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut mcp = Self {
            child,
            stdin,
            lines,
            next_id: 0,
            stderr_path,
        };
        mcp.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }),
        );
        mcp.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        mcp
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write request");
        self.stdin.flush().expect("flush request");
    }

    /// Send a request and return its response. Every line the server prints to
    /// stdout must be a JSON-RPC message: a stray line fails the test.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + LIMIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).unwrap_or_else(|_| {
                panic!(
                    "no reply to {method} within {LIMIT:?}; stderr: {}",
                    self.stderr()
                )
            });
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("stdout carried a line that is not JSON: {line:?}"));
            if value.get("method").is_none() && value["id"] == json!(id) {
                return value;
            }
        }
    }

    /// A `search` with `refresh: true`; returns the reply text.
    fn refreshing_search(&mut self, term: &str) -> String {
        let response = self.request(
            "tools/call",
            json!({"name": "search", "arguments": {"query": term, "refresh": true}}),
        );
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A refresh walks an outside collection, still indexes it, and names it once
/// on stderr however many refreshes follow; stdout stays protocol only.
#[test]
fn ac_9_mcp_refresh_warns_once_on_stderr() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    project.settle();
    std::fs::write(outside.path.join("fresh.md"), "quaggaword arrives\n").expect("write doc");
    std::fs::write(outside.path.join("later.md"), "zebraword arrives\n").expect("write doc");

    let mut mcp = Mcp::start(&project);
    let first = mcp.refreshing_search("quaggaword");
    let second = mcp.refreshing_search("zebraword");

    assert!(first.contains("ext/fresh.md"), "{first}");
    assert!(second.contains("ext/later.md"), "{second}");
    let stderr = mcp.stderr();
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(!stderr.contains("allow-external-paths"));
}

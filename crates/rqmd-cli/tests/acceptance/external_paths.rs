//! Stage 2 of tylern91/rqmd#227: on a project-local `.rqmd/` index, `rqmd update`,
//! `rqmd watch` and the MCP `refresh: true` pass skip a collection whose
//! directory resolves outside the project, naming it once on stderr, unless
//! `--allow-external-paths` is given. A skip indexes nothing, prunes nothing and
//! does not change the exit status. An explicit `--index-dir` and the global
//! index are unchanged, and a path that does not exist keeps only its
//! `directory not found` warning. `collection list`, `collection show`,
//! `doctor` and `status` report such a collection. Design:
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

/// The text every warning for a skipped outside collection ends with.
const TAIL: &str = "outside the project directory; pass --allow-external-paths to index it";

/// What `collection list`, `collection show` and `doctor` append to its path.
const MARKER: &str = "(outside project; skipped)";

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

    /// A directory inside the project with one document, added as a collection.
    fn add_inside(&self, name: &str, body: &str) -> PathBuf {
        let dir = self.dir().join(name);
        std::fs::create_dir(&dir).expect("mkdir inside collection");
        std::fs::write(dir.join("doc.md"), body).expect("write doc");
        self.add(name, &dir);
        dir
    }

    /// `rqmd <args>` from the project with no `--index-dir`: stdout and stderr.
    fn run(&self, args: &[&str]) -> (String, String) {
        let out = rqmd_in_cwd(&self.dir())
            .args(args)
            .output()
            .expect("run rqmd");
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
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

/// An update skips a collection outside the project, naming it once and the
/// flag that lifts the skip, and exits 0.
#[test]
fn ac_1_update_skips_an_outside_collection_and_names_the_flag() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword lives outside\n");
    project.add("ext", &outside.path);
    // `collection add` indexes what is there; this one only an update could pick up.
    std::fs::write(outside.path.join("late.md"), "lateword\n").expect("write doc");

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("WARN: skipping collection 'ext' (")
            && found[0].contains(outside.path.to_str().unwrap()),
        "{}",
        found[0]
    );
    assert!(
        !project.finds("lateword", "ext/late.md"),
        "must not be indexed"
    );
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

/// A relative stored path is read against the project directory first, so
/// `../sibling` is skipped and `inner` is not.
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
    std::fs::write(sibling.join("late.md"), "siblinglate\n").expect("write doc");
    std::fs::write(inner.join("late.md"), "innerlate\n").expect("write doc");

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("collection 'out' (../sibling)"),
        "{}",
        found[0]
    );
    assert!(!project.finds("siblinglate", "out/late.md"));
    assert!(project.finds("innerlate", "in/late.md"));
}

/// A symlinked root is judged by where it points, not where it sits: a link in
/// the project to an outside directory is skipped, a link outside to an inside
/// directory is not.
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
    std::fs::write(outside.path.join("late.md"), "linklate\n").expect("write doc");
    std::fs::write(inside.join("late.md"), "reallate\n").expect("write doc");

    let stderr = project.update();

    let found = warnings(&stderr);
    assert_eq!(found.len(), 1, "stderr: {stderr}");
    assert!(
        found[0].contains("collection 'out' (out-link)"),
        "{}",
        found[0]
    );
    assert!(!project.finds("linklate", "out/late.md"));
    assert!(project.finds("reallate", "in/late.md"));
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
    fn start(project: &Project, extra: &[&str]) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let stderr_path = project.parent.path().join("watch.stderr");
        let child = Command::new(program.get_program())
            .current_dir(project.dir())
            .args(["watch", "--interval", "1"])
            .args(extra)
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

/// Watch passes over the same collections on every changed tick, and names a
/// skipped outside one on the first pass only. The inside collection is the
/// witness that later passes ran.
#[test]
fn ac_8_watch_skips_and_warns_once_across_ticks() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    let inside = project.add_inside("notes", "insideword\n");
    age(&inside.join("doc.md"));
    project.settle();

    let watch = Watch::start(&project, &[]);
    assert!(
        wait_until(LIMIT, || watch.stderr().contains(TAIL)),
        "no warning; stderr: {}",
        watch.stderr()
    );
    // Two more changes in each, each seen by a later pass.
    for (file, word) in [("two.md", "secondword"), ("three.md", "thirdword")] {
        std::fs::write(outside.path.join(file), format!("{word}\n")).expect("write doc");
        std::fs::write(inside.join(file), format!("{word}\n")).expect("write doc");
        assert!(
            wait_until(LIMIT, || project.finds(word, &format!("notes/{file}"))),
            "{file} never became searchable; stderr: {}",
            watch.stderr()
        );
    }

    let stderr = watch.stderr();
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(!project.finds("secondword", "ext/two.md"));
    assert!(!project.finds("thirdword", "ext/three.md"));
}

/// `rqmd watch --allow-external-paths` walks the outside collection and says nothing.
#[test]
fn ac_12_watch_allow_external_paths_indexes_an_outside_collection() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    project.settle();

    let watch = Watch::start(&project, &["--allow-external-paths"]);
    std::fs::write(outside.path.join("two.md"), "secondword\n").expect("write doc");
    assert!(
        wait_until(LIMIT, || project.finds("secondword", "ext/two.md")),
        "never became searchable; stderr: {}",
        watch.stderr()
    );
    assert!(
        !watch.stderr().contains("outside the project"),
        "stderr: {}",
        watch.stderr()
    );
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
    fn start(project: &Project, extra: &[&str]) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let stderr_path = project.parent.path().join("mcp.stderr");
        let mut child = Command::new(program.get_program())
            .current_dir(project.dir())
            .args(["mcp", "--allow-refresh"])
            .args(extra)
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

/// `--allow-refresh` lets a refresh index the inside collection but does not
/// imply `--allow-external-paths`: the outside one is skipped, named once on
/// stderr however many refreshes follow, and stdout stays protocol only.
#[test]
fn ac_9_mcp_refresh_skips_and_warns_once_on_stderr() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    let inside = project.add_inside("notes", "insideword\n");
    age(&inside.join("doc.md"));
    project.settle();
    for dir in [&outside.path, &inside] {
        std::fs::write(dir.join("fresh.md"), "quaggaword arrives\n").expect("write doc");
        std::fs::write(dir.join("later.md"), "zebraword arrives\n").expect("write doc");
    }

    let mut mcp = Mcp::start(&project, &[]);
    let first = mcp.refreshing_search("quaggaword");
    let second = mcp.refreshing_search("zebraword");

    assert!(first.contains("notes/fresh.md"), "{first}");
    assert!(second.contains("notes/later.md"), "{second}");
    assert!(
        !first.contains("ext/fresh.md") && !second.contains("ext/later.md"),
        "{first} {second}"
    );
    let stderr = mcp.stderr();
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
}

/// `rqmd mcp --allow-external-paths` lets a refresh index the outside collection.
#[test]
fn ac_13_mcp_allow_external_paths_refreshes_an_outside_collection() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    project.settle();
    std::fs::write(outside.path.join("fresh.md"), "quaggaword arrives\n").expect("write doc");

    let mut mcp = Mcp::start(&project, &["--allow-external-paths"]);
    let first = mcp.refreshing_search("quaggaword");

    assert!(first.contains("ext/fresh.md"), "{first}");
    let stderr = mcp.stderr();
    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");
}

/// Stops a background daemon left behind by the test, pass or fail.
struct StopDaemon(PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = rqmd(&self.0).args(["mcp", "stop", "--force"]).output();
    }
}

/// An `rqmd mcp --daemon` started from the project, with a session open.
struct Daemon {
    http: reqwest::blocking::Client,
    url: String,
    session: String,
    next_id: u64,
    log_path: PathBuf,
    _stop: StopDaemon,
}

impl Daemon {
    fn start(project: &Project, extra: &[&str]) -> Self {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .expect("bind port 0")
            .local_addr()
            .expect("local addr")
            .port();
        let stop = StopDaemon(project.index());
        let launch = rqmd_in_cwd(&project.dir())
            .args([
                "mcp",
                "--daemon",
                "--allow-refresh",
                "--port",
                &port.to_string(),
            ])
            .args(extra)
            .env_remove("RQMD_MCP_HOST")
            .env_remove("RQMD_MCP_PORT")
            .env_remove("RQMD_MCP_IDLE_EXIT_SECS")
            .output()
            .expect("run rqmd mcp --daemon");
        assert!(launch.status.success(), "daemon launch failed: {launch:?}");

        let http = reqwest::blocking::Client::builder()
            .timeout(LIMIT)
            .build()
            .expect("client");
        let mut daemon = Self {
            http,
            url: format!("http://127.0.0.1:{port}/mcp"),
            session: String::new(),
            next_id: 1,
            log_path: project.index().join("mcp.log"),
            _stop: stop,
        };
        let init = daemon.post(
            false,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0"}
            }}),
        );
        daemon.session = init
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .expect("initialize returned a session id");
        let _ = init.text();
        daemon.post(
            true,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        daemon
    }

    fn post(&self, in_session: bool, body: Value) -> reqwest::blocking::Response {
        let mut request = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_string());
        if in_session {
            request = request.header("Mcp-Session-Id", &self.session);
        }
        request.send().expect("request reaches the daemon")
    }

    /// A `search` with `refresh: true`; returns the raw reply.
    fn refreshing_search(&mut self, term: &str) -> String {
        self.next_id += 1;
        self.post(
            true,
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": "tools/call", "params": {
                "name": "search", "arguments": {"query": term, "refresh": true}
            }}),
        )
        .text()
        .expect("search reply")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

/// A `--daemon` child has an explicit `--index-dir`, so it gets the project
/// directory from the launching process. Its refresh skips an outside
/// collection and names it once in the daemon log, however many refreshes follow.
#[test]
fn ac_10_daemon_refresh_skips_and_warns_once_in_the_daemon_log() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    let inside = project.add_inside("notes", "insideword\n");
    age(&inside.join("doc.md"));
    project.settle();
    for dir in [&outside.path, &inside] {
        std::fs::write(dir.join("fresh.md"), "quaggaword arrives\n").expect("write doc");
        std::fs::write(dir.join("later.md"), "zebraword arrives\n").expect("write doc");
    }

    let mut daemon = Daemon::start(&project, &[]);
    let first = daemon.refreshing_search("quaggaword");
    let second = daemon.refreshing_search("zebraword");

    assert!(first.contains("notes/fresh.md"), "{first}");
    assert!(second.contains("notes/later.md"), "{second}");
    assert!(
        !first.contains("ext/fresh.md") && !second.contains("ext/later.md"),
        "{first} {second}"
    );
    let log = daemon.log();
    assert_eq!(warnings(&log).len(), 1, "daemon log: {log}");
}

/// `rqmd mcp --daemon --allow-external-paths`: the daemon keeps its starter's
/// decision and refreshes the outside collection.
#[test]
fn ac_14_daemon_allow_external_paths_refreshes_an_outside_collection() {
    let project = Project::new();
    let outside = fixture_collection("ext", "firstword\n");
    age(&outside.path.join("doc.md"));
    project.add("ext", &outside.path);
    project.settle();
    std::fs::write(outside.path.join("fresh.md"), "quaggaword arrives\n").expect("write doc");

    let mut daemon = Daemon::start(&project, &["--allow-external-paths"]);
    let first = daemon.refreshing_search("quaggaword");

    assert!(first.contains("ext/fresh.md"), "{first}");
    let log = daemon.log();
    assert!(!log.contains("outside the project"), "daemon log: {log}");
}

/// `rqmd update --allow-external-paths` indexes the outside collection, says
/// nothing about it, and applies to that run only.
#[test]
fn ac_11_update_allow_external_paths_indexes_an_outside_collection() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword lives outside\n");
    project.add("ext", &outside.path);

    let (_, stderr) = project.run(&["update", "--allow-external-paths"]);

    assert!(!stderr.contains("outside the project"), "stderr: {stderr}");
    assert!(project.finds("extword", "ext/doc.md"));
    std::fs::write(outside.path.join("later.md"), "laterword\n").expect("write doc");
    let stderr = project.update();
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(!project.finds("laterword", "ext/later.md"));
}

/// A skipped collection is not an error: the run goes on with the others and
/// exits 0, as it does for a missing directory.
#[test]
fn ac_15_a_skipped_collection_does_not_fail_the_run_or_stop_the_others() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword\n");
    project.add("ext", &outside.path);
    let inside = project.add_inside("notes", "insideword\n");
    std::fs::write(inside.join("late.md"), "insidelate\n").expect("write doc");

    let (stdout, stderr) = project.run(&["update"]);
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(stdout.contains("notes"), "stdout: {stdout}");
    assert!(project.finds("insidelate", "notes/late.md"));

    // Scoped to the skipped collection alone, the exit status is still 0.
    let (_, stderr) = project.run(&["update", "-c", "ext"]);
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
}

/// Documents a collection already holds stay searchable when a later update
/// skips it: nothing is pruned, even for a file deleted from the directory.
#[test]
fn ac_16_a_skip_does_not_prune_existing_documents() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword lives outside\n");
    std::fs::write(outside.path.join("gone.md"), "goneword\n").expect("write doc");
    project.add("ext", &outside.path);
    project.run(&["update", "--allow-external-paths"]);
    assert!(project.finds("extword", "ext/doc.md"));
    assert!(project.finds("goneword", "ext/gone.md"));
    std::fs::remove_file(outside.path.join("gone.md")).expect("delete doc");

    let stderr = project.update();

    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(project.finds("extword", "ext/doc.md"), "pruned");
    assert!(project.finds("goneword", "ext/gone.md"), "pruned");
}

/// The skip comes before the collection's update hook, which would otherwise
/// run in the outside directory; with the flag, it runs.
#[test]
fn ac_17_a_skipped_collection_runs_no_update_hook() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword\n");
    project.add("ext", &outside.path);
    let ran = project.parent.path().join("hook-ran");
    rqmd(&project.index())
        .current_dir(project.dir())
        .args(["collection", "update-cmd", "ext"])
        .arg(format!("touch {}", ran.display()))
        .assert()
        .success();

    let (_, stderr) = project.run(&["update", "--run-hooks"]);
    assert_eq!(warnings(&stderr).len(), 1, "stderr: {stderr}");
    assert!(!ran.exists(), "the hook ran for a skipped collection");

    project.run(&["update", "--run-hooks", "--allow-external-paths"]);
    assert!(ran.exists(), "the hook did not run with the flag");
}

/// `collection list`, `collection show`, `doctor` and `status` report the
/// outside collection on a project-local index, and only that one.
#[test]
fn ac_18_list_show_doctor_and_status_mark_an_outside_collection() {
    let project = Project::new();
    let outside = fixture_collection("ext", "extword\n");
    project.add("ext", &outside.path);
    project.add_inside("notes", "insideword\n");
    let line_with = |text: &str, needle: &str| {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_default()
            .to_owned()
    };

    let (list, _) = project.run(&["collection", "list"]);
    assert!(line_with(&list, "ext").contains(MARKER), "{list}");
    assert!(!line_with(&list, "notes").contains(MARKER), "{list}");

    let (show, _) = project.run(&["collection", "show", "ext"]);
    assert!(line_with(&show, "Path:").contains(MARKER), "{show}");
    let (show, _) = project.run(&["collection", "show", "notes"]);
    assert!(!show.contains(MARKER), "{show}");

    let (doctor, _) = project.run(&["doctor"]);
    assert!(line_with(&doctor, "ext —").contains(MARKER), "{doctor}");
    assert!(!line_with(&doctor, "notes —").contains(MARKER), "{doctor}");
    assert!(doctor.contains("1 collection(s) outside"), "{doctor}");

    let (status, _) = project.run(&["status"]);
    assert!(status.contains("Outside project: 1 skipped"), "{status}");

    // An explicit index is the caller's own choice: no marker, no count.
    for args in [
        &["collection", "list"][..],
        &["collection", "show", "ext"],
        &["doctor"],
        &["status"],
    ] {
        let out = rqmd(&project.index())
            .current_dir(project.dir())
            .args(args)
            .output()
            .expect("run rqmd");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            !text.contains(MARKER) && !text.contains("Outside project"),
            "{args:?}: {text}"
        );
    }
}

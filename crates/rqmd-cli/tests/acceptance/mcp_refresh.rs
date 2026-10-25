//! `refresh: true` on the MCP `search` and `query` tools: before searching, run
//! the incremental full-text pass `rqmd update` does for the requested
//! collections. It never runs update hooks, never embeds, leaves the results
//! unrefreshed with a one-line note while another process holds the index lock
//! or the full-text index needs a rebuild, and writes nothing but protocol
//! messages to stdout. Without it the server behaves as before.
//!
//! Every test drives a real `rqmd mcp` process over stdio. `query` needs a
//! model, so its wiring is covered by the stub-backend tests in `rqmd-mcp`;
//! this tier covers the pass itself through `search`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, rqmd};

/// Upper bound for one reply from the server.
const LIMIT: Duration = Duration::from_secs(60);

/// An aged mtime keeps the stat-skip from re-reading the file on every pass,
/// so a missed change cannot be hidden by a fresh stamp.
fn age(file: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(file)
        .and_then(|f| f.set_modified(old))
        .expect("age file");
}

/// An index with collection `c` holding `doc.md` (`body`) and `gone.md`, settled
/// by two updates: a fresh index needs its one-time full-text rebuild first, and
/// a refresh skips while that is pending.
fn setup(body: &str) -> (TempDir, Fixture) {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("c", body);
    std::fs::write(fx.path.join("gone.md"), "goneword lives here\n").expect("write gone.md");
    age(&fx.path.join("doc.md"));
    age(&fx.path.join("gone.md"));
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "c"])
        .assert()
        .success();
    for _ in 0..2 {
        rqmd(index_dir.path()).arg("update").assert().success();
    }
    (index_dir, fx)
}

/// A running `rqmd mcp` (stdio) that has finished the handshake.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
    stderr_path: PathBuf,
}

/// What one tool call returned.
struct Reply {
    is_error: bool,
    text: String,
}

impl Mcp {
    fn start(index_dir: &Path) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let stderr_path = index_dir.join("mcp.stderr");
        let mut child = Command::new(program.get_program())
            .arg("--index-dir")
            .arg(index_dir)
            .arg("mcp")
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

    /// Send a request and return the response with its id. Every line the
    /// server prints must be a JSON-RPC message: a stray line fails the test.
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
                    std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
                )
            });
            let value: Value = serde_json::from_str(&line)
                .unwrap_or_else(|_| panic!("stdout carried a line that is not JSON: {line:?}"));
            if value.get("method").is_none() && value["id"] == json!(id) {
                return value;
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Reply {
        let response = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &response["result"];
        assert!(result.is_object(), "no result: {response}");
        Reply {
            is_error: result["isError"] == json!(true),
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }
    }

    fn search(&mut self, term: &str, refresh: Option<bool>) -> Reply {
        let mut args = json!({"query": term});
        if let Some(refresh) = refresh {
            args["refresh"] = json!(refresh);
        }
        self.call("search", args)
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn lock_dir(index_dir: &Path) -> PathBuf {
    index_dir.join(".rqmd-write.lock")
}

/// Make the index look locked by a live process (this test process).
fn hold_lock(index_dir: &Path) -> PathBuf {
    let dir = lock_dir(index_dir);
    std::fs::create_dir(&dir).expect("create lock dir");
    std::fs::write(dir.join("pid"), std::process::id().to_string()).expect("write lock pid");
    dir
}

/// The number after `label` in a `status` reply.
fn status_count(status: &str, label: &str) -> i64 {
    status
        .lines()
        .find_map(|l| l.trim().strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no '{label}' in status: {status}"))
}

/// Without `refresh`, or with `refresh: false`, a file written after the last
/// update stays invisible and the reply is the same either way.
#[test]
fn ac_1_without_refresh_nothing_is_indexed() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("doc.md"), "editedword body\n").expect("edit");
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    let omitted = mcp.search("quaggaword", None);
    let off = mcp.search("quaggaword", Some(false));
    let edited = mcp.search("editedword", Some(false));

    assert!(!omitted.is_error && !off.is_error);
    assert!(
        omitted.text.contains("No results found"),
        "got: {}",
        omitted.text
    );
    assert_eq!(omitted.text, off.text);
    assert!(edited.text.contains("No results found"), "{}", edited.text);
}

/// With `refresh: true` an edit, a new file and a deletion are all reflected in
/// the same call, as after `rqmd update`.
#[test]
fn ac_2_refresh_indexes_edits_new_files_and_deletions() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("doc.md"), "editedword body\n").expect("edit");
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");
    std::fs::remove_file(fx.path.join("gone.md")).expect("delete");

    let edited = mcp.search("editedword", Some(true));
    let fresh = mcp.search("quaggaword", Some(true));
    let gone = mcp.search("goneword", Some(true));
    let old = mcp.search("alpha", Some(true));

    assert!(edited.text.contains("c/doc.md"), "got: {}", edited.text);
    assert!(fresh.text.contains("c/fresh.md"), "got: {}", fresh.text);
    assert!(gone.text.contains("No results found"), "got: {}", gone.text);
    assert!(old.text.contains("No results found"), "got: {}", old.text);
    for reply in [&edited, &fresh, &gone, &old] {
        assert!(!reply.is_error);
        assert!(!reply.text.contains("Note:"), "got: {}", reply.text);
    }
}

/// A collection's update hook never runs from a refresh.
#[test]
fn ac_3_refresh_never_runs_update_hooks() {
    let (index_dir, fx) = setup("alpha body\n");
    let marker = index_dir.path().join("hook-ran");
    rqmd(index_dir.path())
        .args(["collection", "update-cmd", "c"])
        .arg(format!("touch {}", marker.display()))
        .assert()
        .success();

    // Premise: for this explicit index `rqmd update` does run the hook.
    rqmd(index_dir.path()).arg("update").assert().success();
    assert!(marker.exists(), "update should have run the hook");
    std::fs::remove_file(&marker).expect("reset marker");

    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "hookfreeword arrives\n").expect("write");
    let reply = mcp.search("hookfreeword", Some(true));

    assert!(
        reply.text.contains("c/fresh.md"),
        "the pass never ran: {}",
        reply.text
    );
    assert!(!marker.exists(), "a refresh must not run the update hook");
}

/// While another process holds the index lock the call still succeeds: it
/// returns what the index holds, with one note saying why, and the next call
/// after the lock is gone refreshes.
#[test]
fn ac_4_a_held_lock_gives_results_and_a_note() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");
    let lock = hold_lock(index_dir.path());

    let held_new = mcp.search("quaggaword", Some(true));
    let held_old = mcp.search("alpha", Some(true));

    assert!(!held_new.is_error && !held_old.is_error);
    assert!(
        held_new.text.contains("No results found"),
        "{}",
        held_new.text
    );
    assert!(
        held_new.text.contains("Note: refresh skipped"),
        "{}",
        held_new.text
    );
    assert_eq!(held_new.text.matches("Note:").count(), 1);
    assert!(held_old.text.contains("c/doc.md"), "{}", held_old.text);
    assert!(
        held_old.text.contains("Note: refresh skipped"),
        "{}",
        held_old.text
    );

    std::fs::remove_dir_all(&lock).expect("release lock");
    let freed = mcp.search("quaggaword", Some(true));
    assert!(freed.text.contains("c/fresh.md"), "{}", freed.text);
    assert!(!freed.text.contains("Note:"), "{}", freed.text);
    assert!(
        !lock_dir(index_dir.path()).exists(),
        "a refresh must release the lock"
    );
}

/// While the full-text index needs a rebuild a refresh does not index into the
/// old one: the call succeeds with the unrefreshed results and a note that names
/// the fix.
#[test]
fn ac_5_a_pending_rebuild_skips_the_pass_with_a_note() {
    let (index_dir, fx) = setup("alpha body\n");
    {
        let db = Connection::open(index_dir.path().join("index.sqlite")).expect("open db");
        db.execute(
            "INSERT OR REPLACE INTO store_config(key, value) VALUES ('fts_tokenizer_version', '0')",
            [],
        )
        .expect("mark the full-text index as built by an older tokenizer");
    }
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    let paused = mcp.search("quaggaword", Some(true));

    assert!(!paused.is_error);
    assert!(paused.text.contains("No results found"), "{}", paused.text);
    assert!(paused.text.contains("needs a rebuild"), "{}", paused.text);
    assert!(paused.text.contains("rqmd update"), "{}", paused.text);

    rqmd(index_dir.path()).arg("update").assert().success();
    let resumed = mcp.search("quaggaword", Some(true));
    assert!(resumed.text.contains("c/fresh.md"), "{}", resumed.text);
}

/// Bad input is handled as before: a `refresh` that is not a boolean is refused
/// as a parameter error that names the type, and an unknown collection returns
/// the same reply with the flag on or off, without indexing any collection.
#[test]
fn ac_6_bad_input_is_unchanged() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    let wrong_type = mcp.call("search", json!({"query": "alpha", "refresh": "yes"}));
    assert!(wrong_type.is_error, "{}", wrong_type.text);
    assert!(
        wrong_type.text.contains("expected a boolean"),
        "{}",
        wrong_type.text
    );

    let args =
        |refresh: bool| json!({"query": "alpha", "collections": ["nope"], "refresh": refresh});
    let off = mcp.call("search", args(false));
    let on = mcp.call("search", args(true));
    assert_eq!(off.is_error, on.is_error);
    assert_eq!(off.text, on.text);
    // A name that matches no collection refreshes nothing.
    let unrefreshed = mcp.search("quaggaword", None);
    assert!(
        unrefreshed.text.contains("No results found"),
        "{}",
        unrefreshed.text
    );
}

/// A refresh that indexes files prints nothing but protocol messages to
/// stdout, which the client parses line by line.
#[test]
fn ac_7_a_refresh_writes_only_protocol_messages_to_stdout() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    let reply = mcp.search("quaggaword", Some(true));
    assert!(reply.text.contains("c/fresh.md"), "{}", reply.text);

    // `request` fails on any non-JSON line, including one printed before it.
    let status = mcp.call("status", json!({}));
    assert!(!status.is_error, "{}", status.text);
}

/// A refresh indexes for keyword search only: the document count grows, the
/// vector count does not and the new document waits for `rqmd embed`.
#[test]
fn ac_8_a_refresh_leaves_vectors_to_embed() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    let before = mcp.call("status", json!({})).text;
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    let reply = mcp.search("quaggaword", Some(true));
    assert!(reply.text.contains("c/fresh.md"), "{}", reply.text);
    let after = mcp.call("status", json!({})).text;

    assert_eq!(
        status_count(&after, "Docs:"),
        status_count(&before, "Docs:") + 1
    );
    assert_eq!(
        status_count(&after, "Vectors:"),
        status_count(&before, "Vectors:")
    );
    assert_eq!(
        status_count(&after, "Pending embed:"),
        status_count(&before, "Pending embed:") + 1
    );
}

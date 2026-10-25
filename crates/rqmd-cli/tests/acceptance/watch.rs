//! `rqmd watch` keeps the full-text index current while it runs: new, edited
//! and deleted files show up in `rqmd search` without a manual `update`. It
//! never runs update hooks, never embeds, waits instead of failing while another
//! process holds the index lock, pauses while the full-text index needs a
//! rebuild, and stops cleanly on SIGINT or SIGTERM.
//!
//! Every test runs a real `rqmd watch --interval 1` process and waits for
//! markers or for search results with a bounded retry, never a fixed sleep.

#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::{Fixture, fixture_collection, rqmd};

/// Upper bound for anything the watcher is expected to do on its own. With a
/// one-second interval it is several passes of headroom, not a delay.
const LIMIT: Duration = Duration::from_secs(30);

/// How long a "this must not happen" check watches for the event: more than two
/// intervals.
const QUIET: Duration = Duration::from_millis(2500);

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

/// A running `rqmd watch`, with its stdout and stderr lines collected.
struct Watch {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Watch {
    fn start(index_dir: &Path, extra: &[&str]) -> Self {
        let program = assert_cmd::Command::cargo_bin("rqmd").expect("rqmd binary");
        let mut command = Command::new(program.get_program());
        command
            .arg("--index-dir")
            .arg(index_dir)
            .args(["watch", "--interval", "1"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // A test run started as a background job inherits SIGINT as ignored, and
        // rqmd leaves an inherited ignore alone.
        // SAFETY: only `signal`, which is async-signal-safe, runs between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn rqmd watch");
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let out_tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = out_tx.send(line);
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Wait until some line of output contains `needle`.
    fn wait_for_line(&mut self, needle: &str, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut next = 0;
        loop {
            while next < self.seen.len() {
                if self.seen[next].contains(needle) {
                    return true;
                }
                next += 1;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) => self.seen.push(line),
                Err(_) => return false,
            }
        }
    }

    fn ready(&mut self) {
        assert!(
            self.wait_for_line("Watching", LIMIT),
            "watch never reported it was running: {:?}",
            self.seen
        );
    }

    /// Running, and its first pass over the collections is done, so a change
    /// made now is one only a later check can see.
    fn ready_after_first_pass(&mut self) {
        self.ready();
        assert!(
            self.wait_for_line("Indexed:", LIMIT),
            "watch never finished its first pass: {:?}",
            self.seen
        );
    }

    /// Passes finished so far: each prints one `Indexed:` summary line.
    fn passes(&mut self) -> usize {
        while let Ok(line) = self.lines.try_recv() {
            self.seen.push(line);
        }
        self.seen
            .iter()
            .filter(|l| l.starts_with("Indexed:"))
            .count()
    }

    /// Passes that added, updated or removed something.
    fn changing_passes(&mut self) -> usize {
        self.passes();
        self.seen
            .iter()
            .filter(|l| l.starts_with("Indexed:"))
            .filter(|l| !(l.starts_with("Indexed: 0 new, 0 updated,") && l.contains(", 0 removed")))
            .count()
    }

    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn wait_exit(&mut self, limit: Duration) -> Option<ExitStatus> {
        wait_until(limit, || self.exited().is_some());
        self.exited()
    }

    fn signal(&self, signal: i32) {
        let pid = i32::try_from(self.child.id()).expect("pid");
        // SAFETY: plain syscall on the rqmd child this test spawned.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Make `file` look an hour old. A file written within the last couple of
/// seconds is not yet trusted by the stat-skip, and watch looks at its
/// collection again on every check until it is, which would hide a missed change.
fn age(file: &Path) {
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(file)
        .and_then(|f| f.set_modified(old))
        .expect("age file");
}

/// An index with collection `c` holding `doc.md` (`body`), settled by two
/// updates: a fresh index needs its one-time full-text rebuild first, and watch
/// stays paused until that has happened.
fn setup(body: &str) -> (TempDir, Fixture) {
    setup_with(body, &[])
}

/// [`setup`] with extra `collection add` arguments.
fn setup_with(body: &str, add_args: &[&str]) -> (TempDir, Fixture) {
    let index_dir = TempDir::new().expect("tempdir");
    let fx = fixture_collection("c", body);
    age(&fx.path.join("doc.md"));
    rqmd(index_dir.path())
        .args(["collection", "add"])
        .arg(&fx.path)
        .args(["--name", "c"])
        .args(add_args)
        .assert()
        .success();
    for _ in 0..2 {
        rqmd(index_dir.path()).arg("update").assert().success();
    }
    (index_dir, fx)
}

/// Whether `rqmd search <term>` lists `file`.
fn finds(index_dir: &Path, term: &str, file: &str) -> bool {
    let out = rqmd(index_dir)
        .args(["search", term])
        .output()
        .expect("run search");
    String::from_utf8_lossy(&out.stdout).contains(file)
}

fn lock_dir(index_dir: &Path) -> PathBuf {
    index_dir.join(".rqmd-write.lock")
}

/// Make the index look locked by a live process (this test process).
fn hold_lock(index_dir: &Path) -> PathBuf {
    let dir = lock_dir(index_dir);
    // A pass that has just printed its summary may still hold the lock.
    assert!(
        wait_until(LIMIT, || std::fs::create_dir(&dir).is_ok()),
        "the lock stayed held"
    );
    std::fs::write(dir.join("pid"), std::process::id().to_string()).expect("write lock pid");
    dir
}

#[test]
fn ac_1_a_new_file_becomes_searchable() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    std::fs::write(fx.path.join("fresh.md"), "quokkaflux appears here\n").expect("write");

    assert!(
        wait_until(LIMIT, || finds(index_dir.path(), "quokkaflux", "fresh.md")),
        "a new file never became searchable; output: {:?}",
        watch.seen
    );
    assert!(watch.exited().is_none(), "watch must keep running");
}

#[test]
fn ac_2_an_edit_replaces_the_searchable_text() {
    let (index_dir, fx) = setup("zanzibarterm lives here\n");
    assert!(finds(index_dir.path(), "zanzibarterm", "doc.md"));
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    std::fs::write(
        fx.path.join("doc.md"),
        "now it says marmaladeterm and more words\n",
    )
    .expect("edit");

    assert!(
        wait_until(LIMIT, || finds(index_dir.path(), "marmaladeterm", "doc.md")),
        "the edited text never became searchable; output: {:?}",
        watch.seen
    );
    assert!(
        !finds(index_dir.path(), "zanzibarterm", "doc.md"),
        "the replaced text must no longer match"
    );
}

#[test]
fn ac_3_a_deleted_file_disappears() {
    let (index_dir, fx) = setup("keeper body\n");
    std::fs::write(fx.path.join("gone.md"), "vanishword was here\n").expect("write");
    age(&fx.path.join("gone.md"));
    rqmd(index_dir.path()).arg("update").assert().success();
    assert!(finds(index_dir.path(), "vanishword", "gone.md"));
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    std::fs::remove_file(fx.path.join("gone.md")).expect("delete");

    assert!(
        wait_until(LIMIT, || !finds(index_dir.path(), "vanishword", "gone.md")),
        "a deleted file stayed searchable; output: {:?}",
        watch.seen
    );
    assert!(
        finds(index_dir.path(), "keeper", "doc.md"),
        "the other file must stay searchable"
    );
}

#[test]
fn ac_4_a_held_lock_makes_watch_wait_instead_of_fail() {
    let (index_dir, fx) = setup("alpha body\n");
    let lock = hold_lock(index_dir.path());
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready();
    std::fs::write(fx.path.join("later.md"), "pangolinword arrives\n").expect("write");

    assert!(
        watch.wait_for_line("waiting for the index lock", LIMIT),
        "watch did not say it was waiting; output: {:?}",
        watch.seen
    );
    assert!(
        watch.wait_exit(QUIET).is_none(),
        "watch must keep running while the lock is held"
    );
    assert!(
        !finds(index_dir.path(), "pangolinword", "later.md"),
        "nothing can be indexed while another process holds the lock"
    );

    std::fs::remove_dir_all(&lock).expect("release lock");
    assert!(
        wait_until(LIMIT, || finds(
            index_dir.path(),
            "pangolinword",
            "later.md"
        )),
        "watch did not catch up once the lock was free; output: {:?}",
        watch.seen
    );
}

#[test]
fn ac_5_sigint_stops_watch_and_leaves_no_lock() {
    let (index_dir, _fx) = setup("alpha body\n");
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    watch.signal(libc::SIGINT);

    let status = watch.wait_exit(LIMIT).expect("watch must exit on SIGINT");
    assert_eq!(status.code(), Some(130), "output: {:?}", watch.seen);
    assert!(watch.wait_for_line("interrupted by SIGINT", LIMIT));
    assert!(!lock_dir(index_dir.path()).exists(), "lock left behind");
}

#[test]
fn ac_6_sigterm_stops_watch_and_leaves_no_lock() {
    let (index_dir, _fx) = setup("alpha body\n");
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    watch.signal(libc::SIGTERM);

    let status = watch.wait_exit(LIMIT).expect("watch must exit on SIGTERM");
    assert_eq!(status.code(), Some(143), "output: {:?}", watch.seen);
    assert!(watch.wait_for_line("interrupted by SIGTERM", LIMIT));
    assert!(!lock_dir(index_dir.path()).exists(), "lock left behind");
}

#[test]
fn ac_7_watch_never_runs_update_hooks() {
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

    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();
    std::fs::write(fx.path.join("fresh.md"), "hookfreeword arrives\n").expect("write");
    assert!(
        wait_until(LIMIT, || finds(
            index_dir.path(),
            "hookfreeword",
            "fresh.md"
        )),
        "a pass never ran; output: {:?}",
        watch.seen
    );

    assert!(!marker.exists(), "watch must not run the update hook");
}

#[test]
fn ac_8_watch_pauses_while_the_full_text_index_needs_a_rebuild() {
    let (index_dir, fx) = setup("alpha body\n");
    {
        let db = Connection::open(index_dir.path().join("index.sqlite")).expect("open db");
        db.execute(
            "INSERT OR REPLACE INTO store_config(key, value) VALUES ('fts_tokenizer_version', '0')",
            [],
        )
        .expect("mark the full-text index as built by an older tokenizer");
    }
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready();
    std::fs::write(fx.path.join("held.md"), "stoatword arrives\n").expect("write");

    assert!(
        watch.wait_for_line("needs a rebuild", LIMIT),
        "watch did not say it was paused; output: {:?}",
        watch.seen
    );
    assert!(
        !wait_until(QUIET, || finds(index_dir.path(), "stoatword", "held.md")),
        "a paused watch must not index into the old full-text index"
    );
    assert!(watch.exited().is_none(), "watch must keep running");

    // The rebuild is `rqmd update`'s job, and watch must not be in its way.
    rqmd(index_dir.path()).arg("update").assert().success();
    std::fs::write(fx.path.join("resumed.md"), "ocelotword arrives\n").expect("write");
    assert!(
        wait_until(LIMIT, || finds(
            index_dir.path(),
            "ocelotword",
            "resumed.md"
        )),
        "watch did not resume after the rebuild; output: {:?}",
        watch.seen
    );
}

#[test]
fn ac_9_interval_has_a_floor_of_one_second() {
    let index_dir = TempDir::new().expect("tempdir");
    rqmd(index_dir.path())
        .args(["watch", "--interval", "0"])
        .timeout(Duration::from_secs(20))
        .assert()
        .failure()
        .stderr(predicates::str::contains("--interval"));
}

#[test]
fn ac_10_an_unknown_collection_is_rejected_up_front() {
    let (index_dir, _fx) = setup("alpha body\n");
    rqmd(index_dir.path())
        .args(["watch", "-c", "nope"])
        .timeout(Duration::from_secs(20))
        .assert()
        .failure()
        .stderr(predicates::str::contains("collection 'nope' not found"));
}

#[test]
fn ac_11_watch_holds_the_lock_only_while_it_indexes() {
    let (index_dir, _fx) = setup("alpha body\n");
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    // Idle watch: an `update` must neither be refused nor wait for it.
    for _ in 0..3 {
        rqmd(index_dir.path())
            .arg("update")
            .timeout(Duration::from_secs(20))
            .assert()
            .success();
        assert!(watch.exited().is_none());
    }
}

/// Write each `(relative path, body)` under `root`, creating folders as needed.
fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (rel, body) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }
}

/// Files that the default mask, the `drafts/**` ignore pattern, the hidden-file
/// rule and the built-in folder list each leave out, with `included.md` last so
/// that it is searchable only after a pass that has seen all of them.
const EXCLUSION_TREE: &[(&str, &str)] = &[
    ("notes.txt", "masktoken\n"),
    ("drafts/ignored.md", "ignoretoken\n"),
    (".hidden-note.md", "hiddentoken\n"),
    (".dotdir/inner.md", "dotdirtoken\n"),
    ("node_modules/pkg/readme.md", "builtintoken\n"),
    ("included.md", "includedtoken\n"),
];

#[test]
fn ac_12_a_burst_of_writes_to_one_file_costs_one_pass() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();
    let before = watch.passes();
    let before_changing = watch.changing_passes();

    // While the lock is held no pass can start, so every write below lands
    // between two checks however long the burst takes.
    let lock = hold_lock(index_dir.path());
    let doc = fx.path.join("doc.md");
    std::fs::write(&doc, "burstzero first revision\n").expect("first write");
    assert!(
        watch.wait_for_line("waiting for the index lock", LIMIT),
        "watch never noticed the change; output: {:?}",
        watch.seen
    );
    for n in 1..=30 {
        std::fs::write(
            &doc,
            format!("burst revision number {n} bursttransient{n}\n"),
        )
        .expect("burst write");
    }
    std::fs::write(&doc, "burstfinal is how it ends\n").expect("last write");
    // Stamped long ago, so the pass that follows is the last one this burst costs.
    age(&doc);
    std::fs::remove_dir_all(&lock).expect("release lock");

    assert!(
        wait_until(LIMIT, || finds(index_dir.path(), "burstfinal", "doc.md")),
        "the last revision never became searchable; output: {:?}",
        watch.seen
    );
    assert!(
        !wait_until(QUIET, || watch.passes() > before + 1),
        "the burst caused more than one pass; output: {:?}",
        watch.seen
    );
    assert_eq!(watch.passes(), before + 1, "output: {:?}", watch.seen);
    assert_eq!(
        watch.changing_passes(),
        before_changing + 1,
        "output: {:?}",
        watch.seen
    );
    for stale in ["burstzero", "bursttransient30"] {
        assert!(
            !finds(index_dir.path(), stale, "doc.md"),
            "{stale} is an intermediate revision and must not be indexed"
        );
    }
}

#[test]
fn ac_13_excluded_files_are_never_indexed_but_an_included_one_is() {
    let (index_dir, fx) = setup_with("alpha body\n", &["--ignore", "drafts/**"]);
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    write_tree(&fx.path, EXCLUSION_TREE);

    assert!(
        wait_until(LIMIT, || finds(
            index_dir.path(),
            "includedtoken",
            "included.md"
        )),
        "the included file never became searchable; output: {:?}",
        watch.seen
    );
    let excluded = [
        ("masktoken", "notes.txt"),
        ("ignoretoken", "ignored.md"),
        ("hiddentoken", ".hidden-note.md"),
        ("dotdirtoken", "inner.md"),
        ("builtintoken", "readme.md"),
    ];
    // The checks span several more passes, since fresh files are looked at
    // again until their stamps settle.
    assert!(
        !wait_until(QUIET, || excluded.iter().any(|(term, file)| finds(
            index_dir.path(),
            term,
            file
        ))),
        "an excluded file was indexed; output: {:?}",
        watch.seen
    );
}

#[test]
fn ac_14_hidden_files_are_indexed_only_when_the_collection_allows_them() {
    let (index_dir, fx) = setup_with("alpha body\n", &["--ignore", "drafts/**", "--hidden"]);
    let mut watch = Watch::start(index_dir.path(), &[]);
    watch.ready_after_first_pass();

    write_tree(&fx.path, EXCLUSION_TREE);

    for (term, file) in [
        ("includedtoken", "included.md"),
        ("hiddentoken", ".hidden-note.md"),
        ("dotdirtoken", "inner.md"),
    ] {
        assert!(
            wait_until(LIMIT, || finds(index_dir.path(), term, file)),
            "{file} never became searchable; output: {:?}",
            watch.seen
        );
    }
    // Hidden files are the only exclusion `--hidden` lifts.
    assert!(
        !wait_until(QUIET, || [
            ("masktoken", "notes.txt"),
            ("ignoretoken", "ignored.md"),
            ("builtintoken", "readme.md"),
        ]
        .iter()
        .any(|(term, file)| finds(index_dir.path(), term, file))),
        "a file excluded by the mask, an ignore pattern or the built-in list was indexed; output: {:?}",
        watch.seen
    );
}

use std::fs;
use std::path::Path;

use predicates::prelude::*;
use predicates::str::contains;
use rusqlite::Connection;
use tempfile::TempDir;

use super::helpers::{refute_vacuous_output, rqmd};

/// Index a tree holding the given `(file name, body)` pairs under one
/// collection named `src`, matching every file.
fn indexed_tree(index_dir: &Path, files: &[(&str, &str)]) -> TempDir {
    let tree = TempDir::new().expect("tempdir");
    for (name, body) in files {
        fs::write(tree.path().join(name), body).expect("write source file");
    }
    rqmd(index_dir)
        .args(["collection", "add"])
        .arg(tree.path())
        .args(["--name", "src", "--mask", "**/*"])
        .assert()
        .success();
    tree
}

fn status(index_dir: &Path) -> assert_cmd::assert::Assert {
    rqmd(index_dir)
        .env_remove("RQMD_INFERENCE_BACKEND")
        .arg("status")
        .assert()
        .success()
}

/// `status` lists every extension that has a grammar: the ones chunked before
/// the registry and the ones it added.
#[test]
fn ac_1_status_lists_every_grammar_extension() {
    let index_dir = TempDir::new().expect("tempdir");
    let _tree = indexed_tree(index_dir.path(), &[("a.md", "# note\n")]);

    let out = status(index_dir.path()).get_output().clone();
    refute_vacuous_output(&out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let grammars = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("Grammars:"))
        .expect("a Grammars line");
    let listed: Vec<&str> = grammars
        .trim_start()
        .trim_start_matches("Grammars:")
        .split(',')
        .map(str::trim)
        .collect();
    for ext in [
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "java", "py", "go", "rs", "cs", "kt", "kts",
        "swift", "tf", "tfvars", "hcl", "sql",
    ] {
        assert!(listed.contains(&ext), "{ext} missing from {listed:?}");
    }
}

/// `status` names the indexed extensions that have no grammar, with their
/// document counts. Extensions with a grammar and prose are not named.
#[test]
fn ac_2_status_names_indexed_extensions_without_a_grammar() {
    let index_dir = TempDir::new().expect("tempdir");
    let _tree = indexed_tree(
        index_dir.path(),
        &[
            ("a.rb", "puts 1\n"),
            ("b.rb", "puts 2\n"),
            ("c.vue", "<template/>\n"),
            ("d.go", "package main\n"),
            ("e.md", "# note\n"),
        ],
    );

    status(index_dir.path())
        .stdout(contains("Without:  .rb (2), .vue (1) (no grammar)"))
        .stdout(contains("Total:    5 files indexed"));
}

/// Default case: when every indexed file has a grammar or is prose, `status`
/// prints no line about missing grammars.
#[test]
fn ac_3_status_has_no_missing_grammar_line_when_every_file_is_covered() {
    let index_dir = TempDir::new().expect("tempdir");
    let _tree = indexed_tree(
        index_dir.path(),
        &[
            ("a.go", "package main\n"),
            ("b.md", "# note\n"),
            ("c.tf", "x = 1\n"),
        ],
    );

    let out = status(index_dir.path())
        .stdout(contains("Total:    3 files indexed"))
        .stdout(contains("Grammars:"))
        .stdout(contains("Without:").not())
        .get_output()
        .clone();
    refute_vacuous_output(&out);
}

/// Fingerprints the previous release wrote under the default embedding model
/// (`ggml-org/embeddinggemma-300M-GGUF`): a `.py` file under the AST marker, a
/// `.go` file under the base fingerprint because it had no grammar.
const PREVIOUS_PY_FINGERPRINT: &str = "72905e";
const PREVIOUS_GO_FINGERPRINT: &str = "5b9014";

fn store_previous_release_vector(index_dir: &Path, file: &str, fingerprint: &str) {
    let conn = Connection::open(index_dir.join("index.sqlite")).expect("open index");
    let updated = conn
        .execute(
            "INSERT INTO content_vectors(hash, seq, pos, model, embed_fingerprint, \
                 total_chunks, embedded_at, vid, chunk_hash) \
             SELECT hash, 0, 0, 'previous-release', ?1, 1, '2026-01-01T00:00:00Z', \
                 (SELECT COUNT(*) FROM content_vectors), 'chunk-' || hash \
             FROM documents WHERE path = ?2",
            [fingerprint, file],
        )
        .expect("seed a vector row");
    assert_eq!(updated, 1, "{file} must be indexed");
}

/// An index embedded by the previous release: its `.py` vectors are still
/// current and need no re-embed, while its `.go` vectors (made without a
/// grammar) are pending until `rqmd embed` re-embeds them once.
#[test]
fn ac_4_only_the_added_languages_need_a_re_embed() {
    let index_dir = TempDir::new().expect("tempdir");
    let _tree = indexed_tree(
        index_dir.path(),
        &[
            ("tool.py", "def run():\n    pass\n"),
            ("main.go", "package main\n"),
        ],
    );
    status(index_dir.path()).stdout(contains("Pending:  2 need embedding"));

    store_previous_release_vector(index_dir.path(), "tool.py", PREVIOUS_PY_FINGERPRINT);
    status(index_dir.path()).stdout(contains("Pending:  1 need embedding"));

    store_previous_release_vector(index_dir.path(), "main.go", PREVIOUS_GO_FINGERPRINT);
    status(index_dir.path())
        .stdout(contains("Pending:  1 need embedding"))
        .stdout(contains("Vectors:  2 embedded"));
}

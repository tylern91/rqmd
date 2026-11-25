//! A collection name that is not registered is an error, not an empty result:
//! `query`, `search` and `vsearch` (CLI) and the MCP `query`, `search` and
//! `multi_get` tools name the unknown collection and the registered ones
//! before any refresh or model load. A registered collection with no matching
//! documents still answers "No results".

use predicates::prelude::*;
use predicates::str::contains;
use serde_json::json;

use super::helpers::rqmd;
use super::mcp_refresh::{Mcp, setup};

/// With a model cache that is empty and offline, a run that reached the model
/// load would fail with a download error instead of the collection error.
fn scoped(index_dir: &std::path::Path, hf_home: &std::path::Path) -> assert_cmd::Command {
    let mut cmd = rqmd(index_dir);
    cmd.env("HF_HOME", hf_home).env("HF_HUB_OFFLINE", "1");
    cmd
}

#[test]
fn ac_1_cli_unknown_collection_fails_naming_it_and_the_registered_ones() {
    let (index_dir, _fx) = setup("alpha body\n");
    let hf_home = tempfile::TempDir::new().expect("tempdir");

    for command in ["query", "search", "vsearch"] {
        scoped(index_dir.path(), hf_home.path())
            .args([command, "alpha", "-c", "nope"])
            .assert()
            .failure()
            .stderr(contains("unknown collection \"nope\""))
            .stderr(contains("registered collections: c"));
    }
    assert!(
        std::fs::read_dir(hf_home.path())
            .expect("read hf home")
            .next()
            .is_none(),
        "a model cache was created"
    );
}

#[test]
fn ac_2_mcp_unknown_collection_is_an_error_before_any_refresh() {
    let (index_dir, fx) = setup("alpha body\n");
    let mut mcp = Mcp::start(index_dir.path());
    std::fs::write(fx.path.join("fresh.md"), "quaggaword arrives\n").expect("write");

    for (tool, args) in [
        (
            "query",
            json!({"query": "alpha", "collections": ["nope"], "refresh": true}),
        ),
        (
            "search",
            json!({"query": "alpha", "collections": ["nope"], "refresh": true}),
        ),
        // A registered name beside the unknown one would let a refresh index
        // fresh.md if the check ran after it.
        (
            "query",
            json!({"query": "alpha", "collections": ["c", "nope"], "refresh": true}),
        ),
        (
            "search",
            json!({"query": "alpha", "collections": ["c", "nope"], "refresh": true}),
        ),
        (
            "multi_get",
            json!({"pattern": "*.md", "collections": ["nope"]}),
        ),
    ] {
        let reply = mcp.call(tool, args);
        assert!(reply.is_error, "{tool}: {}", reply.text);
        assert!(reply.text.contains("\"nope\""), "{tool}: {}", reply.text);
        assert!(
            reply.text.contains("registered collections: c"),
            "{tool}: {}",
            reply.text
        );
    }
    let unrefreshed = mcp.search("quaggaword", None);
    assert!(
        unrefreshed.text.contains("No results found"),
        "{}",
        unrefreshed.text
    );
}

#[test]
fn ac_3_registered_collection_without_matches_still_says_no_results() {
    let (index_dir, _fx) = setup("alpha body\n");

    rqmd(index_dir.path())
        .args(["search", "zzzznomatch", "-c", "c"])
        .assert()
        .success()
        .stdout(contains("No results"));
}

#[test]
fn ac_4_one_unknown_name_among_several_fails_the_whole_call_naming_only_it() {
    let (index_dir, _fx) = setup("alpha body\n");

    rqmd(index_dir.path())
        .args(["search", "alpha", "-c", "c", "-c", "nope"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(contains("unknown collection \"nope\""))
        .stderr(contains("unknown collection \"c\"").not());
}

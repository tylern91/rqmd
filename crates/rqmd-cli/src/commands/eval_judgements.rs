//! `rqmd eval --judgements <file>`: score search quality against a judgement
//! file, on the user's own index (github.com/tylern91/rqmd#153).
//!
//! The index is only read: it is opened through the `open_existing_store*`
//! helpers with `read_only = true`, so a missing index is an error and
//! nothing is created. The built-in six-document gate lives in `eval.rs` and
//! is untouched by this module.
//!
//! Judgement file (documented in docs/CLI.md): JSON
//! `{"queries": [{"query": "...", "relevant": ...}]}`, or JSONL (a `.jsonl`
//! file) holding one query object per line. `relevant` is an array of
//! `"collection/path"` strings (grade 1 each) or an object mapping
//! `"collection/path"` to an integer grade (0 = judged, not relevant).

use anyhow::{Context, Result, bail};
use rqmd_core::eval_metrics as metrics;
use rqmd_core::{Deadline, SearchOptions, SearchResult, Store, db};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crate::exit::PartialFailure;
use crate::store;

/// nDCG and MRR are always cut at 10, as #129 states its decision rule.
const RANK_CUTOFF: usize = 10;

/// What `rqmd eval --judgements` was asked to do.
pub struct JudgementEval<'a> {
    pub judgements: &'a Path,
    pub mode: &'a str,
    /// Cutoff for Recall@K and Hit@K.
    pub k: usize,
    pub json: bool,
    pub verbose: bool,
    /// Hybrid only: skip the rerank stage.
    pub no_rerank: bool,
    /// Hybrid only: skip query expansion.
    pub no_expand: bool,
}

// ── Judgement file ────────────────────────────────────────────────────────────

/// One query and the documents judged for it, keyed `collection/path`.
#[derive(Debug, PartialEq, Eq)]
struct JudgedQuery {
    query: String,
    grades: BTreeMap<String, u32>,
}

/// Split `collection/path` at the first `/` (collection names never contain
/// one). Both halves must be non-empty.
fn split_key(key: &str) -> Option<(&str, &str)> {
    key.split_once('/')
        .filter(|(collection, path)| !collection.is_empty() && !path.is_empty())
}

fn parse_grade(key: &str, value: &Value, at: &str) -> Result<u32> {
    value
        .as_u64()
        .and_then(|g| u32::try_from(g).ok())
        .with_context(|| {
            format!("{at}: grade of \"{key}\" must be a non-negative integer, got {value}")
        })
}

fn parse_query(value: &Value, at: &str) -> Result<JudgedQuery> {
    let obj = value
        .as_object()
        .with_context(|| format!("{at}: expected an object with \"query\" and \"relevant\""))?;
    if let Some(unknown) = obj.keys().find(|k| *k != "query" && *k != "relevant") {
        bail!("{at}: unknown field \"{unknown}\" (allowed: \"query\", \"relevant\")");
    }
    let query = obj
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .with_context(|| format!("{at}: \"query\" must be a non-empty string"))?
        .to_string();

    let relevant = obj
        .get("relevant")
        .with_context(|| format!("{at}: missing \"relevant\""))?;
    let mut grades = BTreeMap::new();
    match relevant {
        Value::Array(items) => {
            for item in items {
                let key = item.as_str().with_context(|| {
                    format!("{at}: \"relevant\" entries must be strings, got {item}")
                })?;
                if grades.insert(key.to_string(), 1).is_some() {
                    bail!("{at}: \"{key}\" is listed twice in \"relevant\"");
                }
            }
        }
        Value::Object(map) => {
            for (key, grade) in map {
                grades.insert(key.clone(), parse_grade(key, grade, at)?);
            }
        }
        _ => bail!(
            "{at}: \"relevant\" must be an array of \"collection/path\" strings or an object \
             mapping them to integer grades"
        ),
    }
    for key in grades.keys() {
        if split_key(key).is_none() {
            bail!("{at}: \"{key}\" is not of the form \"collection/path\"");
        }
    }
    if !grades.values().any(|g| *g >= 1) {
        bail!("{at}: \"relevant\" needs at least one document with a grade of 1 or more");
    }
    Ok(JudgedQuery { query, grades })
}

/// Parse a judgement file's text. `jsonl` selects one query object per line
/// (blank lines are skipped); otherwise the text is one JSON document.
fn parse_judgements(text: &str, jsonl: bool) -> Result<Vec<JudgedQuery>> {
    let queries = if jsonl {
        let mut queries = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let at = format!("line {}", i + 1);
            let value: Value =
                serde_json::from_str(line).with_context(|| format!("{at}: not valid JSON"))?;
            queries.push(parse_query(&value, &at)?);
        }
        queries
    } else {
        let doc: Value = serde_json::from_str(text).context("not valid JSON")?;
        let obj = doc
            .as_object()
            .context("top level must be an object with a \"queries\" array")?;
        if let Some(unknown) = obj.keys().find(|k| *k != "queries") {
            bail!("unknown top-level field \"{unknown}\" (allowed: \"queries\")");
        }
        let list = obj
            .get("queries")
            .and_then(Value::as_array)
            .context("top level must have a \"queries\" array")?;
        list.iter()
            .enumerate()
            .map(|(i, v)| parse_query(v, &format!("query #{}", i + 1)))
            .collect::<Result<Vec<_>>>()?
    };
    if queries.is_empty() {
        bail!("the file holds no queries");
    }
    Ok(queries)
}

fn load_judgements(path: &Path) -> Result<Vec<JudgedQuery>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read judgement file {}", path.display()))?;
    let jsonl = path.extension().is_some_and(|e| e == "jsonl");
    parse_judgements(&text, jsonl)
        .with_context(|| format!("malformed judgement file {}", path.display()))
}

// ── Scoring ───────────────────────────────────────────────────────────────────

/// How one scored query came out.
struct QueryScore {
    query: String,
    ndcg: f64,
    reciprocal_rank: f64,
    recall: f64,
    hit: bool,
    latency_ms: f64,
    ranked: Vec<String>,
}

/// `collection/path` keys of `results`, best first, each document once.
fn ranked_keys(results: &[SearchResult]) -> Vec<String> {
    let mut seen = HashSet::new();
    results
        .iter()
        .map(|r| format!("{}/{}", r.collection, r.path))
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

fn score_query(
    query: &str,
    ranked: Vec<String>,
    grades: &HashMap<String, u32>,
    k: usize,
    latency_ms: f64,
) -> QueryScore {
    let relevant: HashSet<String> = grades
        .iter()
        .filter(|(_, g)| **g >= 1)
        .map(|(key, _)| key.clone())
        .collect();
    QueryScore {
        query: query.to_string(),
        ndcg: metrics::ndcg_at_k(&ranked, grades, RANK_CUTOFF),
        reciprocal_rank: metrics::reciprocal_rank_at_k(&ranked, &relevant, RANK_CUTOFF),
        recall: metrics::recall_at_k(&ranked, &relevant, k),
        hit: metrics::hit_at_k(&ranked, &relevant, k),
        latency_ms,
        ranked,
    }
}

// ── Run ───────────────────────────────────────────────────────────────────────

/// Open the index for `mode`. vec and hybrid need stored embeddings and a
/// working inference backend; both are checked up front and fail with a
/// message saying what is missing, instead of scoring empty results as misses.
fn open_for_mode(index_dir: &Path, mode: &str) -> Result<Store> {
    let plain = store::open_existing_store_no_backend(index_dir, true)?;
    store::warn_if_raw_backfill_pending(&plain);
    store::warn_if_fts_rebuild_pending(&plain);
    if mode == "bm25" {
        return Ok(plain);
    }
    if db::fingerprint_breakdown_active(&plain.db)?.is_empty() {
        bail!(
            "mode {mode} needs embeddings, but this index has none: run `rqmd embed` first, or \
             use --mode bm25"
        );
    }
    drop(plain);
    let s = store::open_existing_store_with_backend(index_dir, true).with_context(|| {
        format!("mode {mode} needs an embedding backend and it could not be started")
    })?;
    store::warn_if_fingerprint_stale(&s);
    Ok(s)
}

fn percentile_json(values: &[f64], p: f64) -> Value {
    json!(metrics::percentile(values, p))
}

pub fn run_eval_judgements(index_dir: &Path, opts: &JudgementEval) -> Result<()> {
    if !matches!(opts.mode, "bm25" | "vec" | "hybrid") {
        bail!(
            "unknown mode \"{}\": expected bm25, vec or hybrid",
            opts.mode
        );
    }
    // A malformed file is reported before the index is touched.
    let queries = load_judgements(opts.judgements)?;

    let mut s = open_for_mode(index_dir, opts.mode)?;
    let collections: Vec<String> = db::list_collections(&s.db)?
        .into_iter()
        .map(|c| c.name)
        .collect();
    if collections.is_empty() {
        bail!("the index has no collections to evaluate against");
    }

    // Which judged documents the index holds. A missing one is listed and
    // dropped from its query's judgements; it is never scored as a miss.
    let mut present: HashMap<String, bool> = HashMap::new();
    for key in queries.iter().flat_map(|q| q.grades.keys()) {
        if !present.contains_key(key) {
            let (collection, path) = split_key(key).expect("validated at parse time");
            let found = db::get_active_document_by_filepath(&s.db, collection, path)?.is_some();
            present.insert(key.clone(), found);
        }
    }

    let fetch = RANK_CUTOFF.max(opts.k);
    let mut scores: Vec<QueryScore> = Vec::new();
    let mut skipped: Vec<(String, &str)> = Vec::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut rerank_notice: Option<&'static str> = None;

    for judged in &queries {
        let mut grades: HashMap<String, u32> = HashMap::new();
        for (key, grade) in &judged.grades {
            if present[key] {
                grades.insert(key.clone(), *grade);
            } else {
                missing.push((judged.query.clone(), key.clone()));
            }
        }
        if !grades.values().any(|g| *g >= 1) {
            skipped.push((
                judged.query.clone(),
                "every relevant document is missing from the index",
            ));
            continue;
        }

        let started = Instant::now();
        let results = match opts.mode {
            "bm25" => s.search_fts_multi(&judged.query, fetch, Some(&collections)),
            "vec" => s.search_vec_multi(&judged.query, fetch, Some(&collections)),
            _ => s
                .hybrid_query_outcome_with(
                    &judged.query,
                    None,
                    fetch,
                    Some(&collections),
                    opts.no_rerank,
                    opts.no_expand,
                    Deadline::none(),
                    false,
                    &SearchOptions::default(),
                )
                .map(|outcome| {
                    if rerank_notice.is_none() {
                        rerank_notice = outcome.rerank.notice();
                    }
                    outcome.results
                }),
        }
        .with_context(|| format!("search failed for query \"{}\"", judged.query))?;
        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

        scores.push(score_query(
            &judged.query,
            ranked_keys(&results),
            &grades,
            opts.k,
            latency_ms,
        ));
    }

    let ndcgs: Vec<f64> = scores.iter().map(|q| q.ndcg).collect();
    let rrs: Vec<f64> = scores.iter().map(|q| q.reciprocal_rank).collect();
    let recalls: Vec<f64> = scores.iter().map(|q| q.recall).collect();
    let hits: Vec<f64> = scores.iter().map(|q| f64::from(u8::from(q.hit))).collect();
    let latencies: Vec<f64> = scores.iter().map(|q| q.latency_ms).collect();
    let k = opts.k;

    if let Some(notice) = rerank_notice {
        eprintln!("rqmd: warning: {notice}");
    }

    if opts.json {
        let report = json!({
            "judgements": opts.judgements.display().to_string(),
            "mode": opts.mode,
            "k": k,
            "queries_total": queries.len(),
            "queries_scored": scores.len(),
            "queries_skipped": skipped
                .iter()
                .map(|(q, why)| json!({"query": q, "reason": why}))
                .collect::<Vec<_>>(),
            "metrics": {
                "ndcg_at_10": metrics::mean(&ndcgs),
                "mrr_at_10": metrics::mean(&rrs),
                "recall_at_k": metrics::mean(&recalls),
                "hit_at_k": metrics::mean(&hits),
            },
            "latency_ms": {
                "p50": percentile_json(&latencies, 50.0),
                "p95": percentile_json(&latencies, 95.0),
                "max": percentile_json(&latencies, 100.0),
                "mean": metrics::mean(&latencies),
            },
            "rerank_notice": rerank_notice,
            "missing_documents": missing
                .iter()
                .map(|(q, d)| json!({"query": q, "document": d}))
                .collect::<Vec<_>>(),
            "per_query": scores
                .iter()
                .map(|q| json!({
                    "query": q.query,
                    "ndcg_at_10": q.ndcg,
                    "rr_at_10": q.reciprocal_rank,
                    "recall_at_k": q.recall,
                    "hit_at_k": q.hit,
                    "latency_ms": q.latency_ms,
                    "ranked": q.ranked.iter().take(RANK_CUTOFF.max(k)).collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("rqmd eval: judgement file {}", opts.judgements.display());
        println!(
            "Mode: {}   Queries: {} scored, {} skipped, {} total   K: {k}",
            opts.mode,
            scores.len(),
            skipped.len(),
            queries.len()
        );
        println!();
        println!("  nDCG@10      {:.4}", metrics::mean(&ndcgs));
        println!("  MRR@10       {:.4}", metrics::mean(&rrs));
        println!("  Recall@{k:<5} {:.4}", metrics::mean(&recalls));
        println!("  Hit@{k:<8} {:.4}", metrics::mean(&hits));
        if let (Some(p50), Some(p95), Some(max)) = (
            metrics::percentile(&latencies, 50.0),
            metrics::percentile(&latencies, 95.0),
            metrics::percentile(&latencies, 100.0),
        ) {
            println!("  Latency ms   p50 {p50:.1}   p95 {p95:.1}   max {max:.1}");
        }
        if opts.verbose {
            println!("\n  Per query:");
            for q in &scores {
                println!(
                    "    nDCG {:.4}  RR {:.4}  Recall {:.4}  {}  {:.1} ms  {:?}",
                    q.ndcg,
                    q.reciprocal_rank,
                    q.recall,
                    if q.hit { "hit " } else { "miss" },
                    q.latency_ms,
                    q.query
                );
            }
        }
        if !skipped.is_empty() {
            println!("\n  Skipped queries (not scored):");
            for (query, why) in &skipped {
                println!("    {query:?}: {why}");
            }
        }
        if !missing.is_empty() {
            println!(
                "\n  Judged documents missing from the index ({}), left out of the scoring:",
                missing.len()
            );
            for (query, doc) in &missing {
                println!("    {doc}   (query {query:?})");
            }
        }
    }

    if scores.is_empty() {
        bail!("no query could be scored: every judged relevant document is missing from the index");
    }
    if !missing.is_empty() {
        return Err(PartialFailure(format!(
            "{} judged document(s) are missing from the index and were not scored; fix the \
             judgement file or re-index",
            missing.len()
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<JudgedQuery>> {
        parse_judgements(text, false)
    }

    fn err_text(text: &str) -> String {
        format!("{:#}", parse(text).unwrap_err())
    }

    #[test]
    fn parses_list_and_graded_forms() {
        let q = parse(
            r#"{"queries": [
                {"query": "a", "relevant": ["notes/x.md", "notes/y.md"]},
                {"query": "b", "relevant": {"notes/x.md": 3, "docs/z.md": 0}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].grades["notes/x.md"], 1);
        assert_eq!(q[1].grades["notes/x.md"], 3);
        assert_eq!(q[1].grades["docs/z.md"], 0);
    }

    #[test]
    fn parses_jsonl_and_skips_blank_lines() {
        let text = "{\"query\":\"a\",\"relevant\":[\"c/p.md\"]}\n\n{\"query\":\"b\",\"relevant\":{\"c/q.md\":2}}\n";
        let q = parse_judgements(text, true).unwrap();
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn rejects_malformed_input_with_a_reason() {
        assert!(err_text("not json").contains("not valid JSON"));
        assert!(err_text("[]").contains("top level must be an object"));
        assert!(err_text(r#"{"queries": []}"#).contains("no queries"));
        assert!(err_text(r#"{"queries": [], "x": 1}"#).contains("unknown top-level field"));
        assert!(
            err_text(r#"{"queries": [{"query": "", "relevant": ["a/b"]}]}"#)
                .contains("query #1: \"query\" must be a non-empty string")
        );
        assert!(err_text(r#"{"queries": [{"query": "q"}]}"#).contains("missing \"relevant\""));
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": ["nopath"]}]}"#)
                .contains("not of the form \"collection/path\"")
        );
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": {"a/b": -1}}]}"#)
                .contains("non-negative integer")
        );
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": {"a/b": 1.5}}]}"#)
                .contains("non-negative integer")
        );
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": {"a/b": 0}}]}"#)
                .contains("at least one document with a grade of 1")
        );
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": ["a/b", "a/b"]}]}"#)
                .contains("listed twice")
        );
        assert!(
            err_text(r#"{"queries": [{"query": "q", "relevant": ["a/b"], "relevent": 1}]}"#)
                .contains("unknown field \"relevent\"")
        );
    }

    #[test]
    fn jsonl_errors_name_the_line() {
        let e = format!(
            "{:#}",
            parse_judgements("{\"query\":\"a\",\"relevant\":[\"c/p\"]}\nnope\n", true).unwrap_err()
        );
        assert!(e.contains("line 2"), "{e}");
    }

    #[test]
    fn split_key_needs_both_halves() {
        assert_eq!(split_key("notes/a/b.md"), Some(("notes", "a/b.md")));
        assert_eq!(split_key("/b.md"), None);
        assert_eq!(split_key("notes/"), None);
        assert_eq!(split_key("notes"), None);
    }
}

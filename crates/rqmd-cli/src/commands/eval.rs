//! Search quality evaluation harness.
//!
//! Mirrors the TypeScript `eval.test.ts` fixtures: indexes 6 synthetic documents,
//! runs 24 queries across four difficulty tiers, and scores Hit@K results against
//! the same thresholds used in the TS test suite.
//!
//! Modes:
//!   bm25   — FTS only, no model required (default)
//!   vec    — vector only, requires inference backend
//!   hybrid — BM25 + vector + RRF + rerank, requires inference backend

use anyhow::Result;
use rqmd_core::{Store, StoreConfig};
use rqmd_llm::{create_backend, BackendKind};
use std::path::Path;
use tempfile::TempDir;

// ── Embedded eval corpus ──────────────────────────────────────────────────────
// Six synthetic documents. File stem is used as the expected-doc substring.

struct EvalDoc {
    filename: &'static str,
    content: &'static str,
}

const EVAL_DOCS: &[EvalDoc] = &[
    EvalDoc {
        filename: "api-design-principles.md",
        content: include_str!("../../eval-docs/api-design-principles.md"),
    },
    EvalDoc {
        filename: "distributed-systems-overview.md",
        content: include_str!("../../eval-docs/distributed-systems-overview.md"),
    },
    EvalDoc {
        filename: "machine-learning-primer.md",
        content: include_str!("../../eval-docs/machine-learning-primer.md"),
    },
    EvalDoc {
        filename: "product-launch-retrospective.md",
        content: include_str!("../../eval-docs/product-launch-retrospective.md"),
    },
    EvalDoc {
        filename: "remote-work-policy.md",
        content: include_str!("../../eval-docs/remote-work-policy.md"),
    },
    EvalDoc {
        filename: "startup-fundraising-memo.md",
        content: include_str!("../../eval-docs/startup-fundraising-memo.md"),
    },
];

// ── Eval query set ────────────────────────────────────────────────────────────
// Matches eval.test.ts exactly: 6 queries × 4 difficulty tiers = 24 total.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Difficulty {
    Easy,
    Medium,
    Hard,
    Fusion,
}

struct EvalQuery {
    query: &'static str,
    /// Substring of the expected document's filepath (case-insensitive).
    expected_doc: &'static str,
    difficulty: Difficulty,
}

const EVAL_QUERIES: &[EvalQuery] = &[
    // Easy: exact keyword matches
    EvalQuery {
        query: "API versioning",
        expected_doc: "api-design",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "Series A fundraising",
        expected_doc: "fundraising",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "CAP theorem",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "overfitting machine learning",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "remote work VPN",
        expected_doc: "remote-work",
        difficulty: Difficulty::Easy,
    },
    EvalQuery {
        query: "Project Phoenix retrospective",
        expected_doc: "product-launch",
        difficulty: Difficulty::Easy,
    },
    // Medium: semantic / conceptual queries
    EvalQuery {
        query: "how to structure REST endpoints",
        expected_doc: "api-design",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "raising money for startup",
        expected_doc: "fundraising",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "consistency vs availability tradeoffs",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "how to prevent models from memorizing data",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "working from home guidelines",
        expected_doc: "remote-work",
        difficulty: Difficulty::Medium,
    },
    EvalQuery {
        query: "what went wrong with the launch",
        expected_doc: "product-launch",
        difficulty: Difficulty::Medium,
    },
    // Hard: vague, partial memory, indirect
    EvalQuery {
        query: "nouns not verbs",
        expected_doc: "api-design",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "Sequoia investor pitch",
        expected_doc: "fundraising",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "Raft algorithm leader election",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "F1 score precision recall",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "quarterly team gathering travel",
        expected_doc: "remote-work",
        difficulty: Difficulty::Hard,
    },
    EvalQuery {
        query: "beta program 47 bugs",
        expected_doc: "product-launch",
        difficulty: Difficulty::Hard,
    },
    // Fusion: need both lexical AND semantic signal
    EvalQuery {
        query: "how much runway before running out of money",
        expected_doc: "fundraising",
        difficulty: Difficulty::Fusion,
    },
    EvalQuery {
        query: "datacenter replication sync strategy",
        expected_doc: "distributed-systems",
        difficulty: Difficulty::Fusion,
    },
    EvalQuery {
        query: "splitting data for training and testing",
        expected_doc: "machine-learning",
        difficulty: Difficulty::Fusion,
    },
    EvalQuery {
        query: "JSON response codes error messages",
        expected_doc: "api-design",
        difficulty: Difficulty::Fusion,
    },
    EvalQuery {
        query: "video calls camera async messaging",
        expected_doc: "remote-work",
        difficulty: Difficulty::Fusion,
    },
    EvalQuery {
        query: "CI/CD pipeline testing coverage",
        expected_doc: "product-launch",
        difficulty: Difficulty::Fusion,
    },
];

// ── Thresholds (mirrors eval.test.ts) ────────────────────────────────────────

struct Thresholds {
    easy_hit3: f64,
    medium_hit3: f64,
    hard_hit5: f64,
    overall_hit3: f64,
}

const BM25_THRESHOLDS: Thresholds = Thresholds {
    easy_hit3: 0.80,
    medium_hit3: 0.15,
    hard_hit5: 0.15,
    overall_hit3: 0.40,
};

const VEC_THRESHOLDS: Thresholds = Thresholds {
    easy_hit3: 0.60,
    medium_hit3: 0.40,
    hard_hit5: 0.30,
    overall_hit3: 0.50,
};

const HYBRID_THRESHOLDS: Thresholds = Thresholds {
    easy_hit3: 0.80,
    medium_hit3: 0.50,
    hard_hit5: 0.30,
    overall_hit3: 0.60,
};

// ── Scoring ───────────────────────────────────────────────────────────────────

/// Returns true if any of the top-k results' file paths contain `expected_doc`
/// as a case-insensitive substring. Mirrors TypeScript `matchesExpected`.
fn hit_at_k(results: &[impl AsRef<str>], expected_doc: &str, k: usize) -> bool {
    results
        .iter()
        .take(k)
        .any(|r| r.as_ref().to_ascii_lowercase().contains(expected_doc))
}

fn hit_rate(
    queries: &[&EvalQuery],
    results_fn: &mut impl FnMut(&str) -> Vec<String>,
    k: usize,
) -> f64 {
    let total = queries.len();
    if total == 0 {
        return 0.0;
    }
    let hits: usize = queries
        .iter()
        .filter(|q| hit_at_k(&results_fn(q.query), q.expected_doc, k))
        .count();
    hits as f64 / total as f64
}

// ── Store helpers ─────────────────────────────────────────────────────────────

fn make_temp_store(backend: Box<dyn rqmd_llm::InferenceBackend>) -> Result<(TempDir, Store)> {
    let tmp = TempDir::new()?;
    let config = StoreConfig {
        db_path: tmp.path().join("eval.sqlite"),
        tantivy_dir: tmp.path().join("tantivy"),
        hnsw_path: tmp.path().join("hnsw.usearch"),
    };
    let store = Store::open(config, backend)?;
    Ok((tmp, store))
}

fn index_all_fts_only(store: &mut Store) -> Result<()> {
    for doc in EVAL_DOCS {
        let title = doc
            .content
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_string();
        store.index_document_fts_only("eval-docs", doc.filename, &title, doc.content)?;
    }
    store.flush()?;
    Ok(())
}

fn index_all_with_embed(store: &mut Store) -> Result<()> {
    for doc in EVAL_DOCS {
        let title = doc
            .content
            .lines()
            .next()
            .unwrap_or("")
            .trim_start_matches('#')
            .trim()
            .to_string();
        store.index_document("eval-docs", doc.filename, &title, doc.content)?;
    }
    store.flush()?;
    Ok(())
}

// ── Report ────────────────────────────────────────────────────────────────────

fn pass_fail(rate: f64, threshold: f64) -> &'static str {
    if rate >= threshold {
        "PASS"
    } else {
        "FAIL"
    }
}

fn print_report(mode: &str, thresholds: &Thresholds, results: &EvalResults) {
    println!();
    println!("  ┌─────────────────────────────────────────────────────┐");
    println!("  │  {mode:<51} │");
    println!("  ├──────────────────────┬────────┬───────────┬─────────┤");
    println!("  │  Tier                │  Hit@K │  Score    │  Gate   │");
    println!("  ├──────────────────────┼────────┼───────────┼─────────┤");
    println!(
        "  │  easy      (Hit@3)   │   @3   │  {:>5.1}%  │  {}   │",
        results.easy_hit3 * 100.0,
        pass_fail(results.easy_hit3, thresholds.easy_hit3)
    );
    println!(
        "  │  medium    (Hit@3)   │   @3   │  {:>5.1}%  │  {}   │",
        results.medium_hit3 * 100.0,
        pass_fail(results.medium_hit3, thresholds.medium_hit3)
    );
    println!(
        "  │  hard      (Hit@5)   │   @5   │  {:>5.1}%  │  {}   │",
        results.hard_hit5 * 100.0,
        pass_fail(results.hard_hit5, thresholds.hard_hit5)
    );
    println!(
        "  │  fusion    (Hit@3)   │   @3   │  {:>5.1}%  │  n/a  │",
        results.fusion_hit3 * 100.0
    );
    println!("  ├──────────────────────┼────────┼───────────┼─────────┤");
    println!(
        "  │  overall   (Hit@3)   │   @3   │  {:>5.1}%  │  {}   │",
        results.overall_hit3 * 100.0,
        pass_fail(results.overall_hit3, thresholds.overall_hit3)
    );
    println!("  └──────────────────────┴────────┴───────────┴─────────┘");

    let all_pass = results.easy_hit3 >= thresholds.easy_hit3
        && results.medium_hit3 >= thresholds.medium_hit3
        && results.hard_hit5 >= thresholds.hard_hit5
        && results.overall_hit3 >= thresholds.overall_hit3;
    println!();
    if all_pass {
        println!("  ✓ All gates PASS — quality parity confirmed");
    } else {
        println!("  ✗ One or more gates FAILED — search quality regression");
    }
}

struct EvalResults {
    easy_hit3: f64,
    medium_hit3: f64,
    hard_hit5: f64,
    fusion_hit3: f64,
    overall_hit3: f64,
}

fn score_all(search_fn: &mut impl FnMut(&str) -> Vec<String>) -> EvalResults {
    let easy: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Easy)
        .collect();
    let medium: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Medium)
        .collect();
    let hard: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Hard)
        .collect();
    let fusion: Vec<&EvalQuery> = EVAL_QUERIES
        .iter()
        .filter(|q| q.difficulty == Difficulty::Fusion)
        .collect();

    EvalResults {
        easy_hit3: hit_rate(&easy, search_fn, 3),
        medium_hit3: hit_rate(&medium, search_fn, 3),
        hard_hit5: hit_rate(&hard, search_fn, 5),
        fusion_hit3: hit_rate(&fusion, search_fn, 3),
        overall_hit3: hit_rate(&EVAL_QUERIES.iter().collect::<Vec<_>>(), search_fn, 3),
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_eval(_index_dir: &Path, mode: &str, verbose: bool) -> Result<()> {
    println!("qmd eval — search quality harness");
    println!(
        "Corpus: {} documents, {} queries",
        EVAL_DOCS.len(),
        EVAL_QUERIES.len()
    );

    match mode {
        "vec" | "hybrid" => {
            println!("Loading inference backend (downloads models on first run)...");
            let backend = create_backend(&BackendKind::from_env())?;
            let (_tmp, mut store) = make_temp_store(backend)?;

            println!("Indexing corpus ({} docs)...", EVAL_DOCS.len());
            index_all_with_embed(&mut store)?;
            println!("Indexing complete.");

            if mode == "vec" {
                println!("\nMode: vector search (vsearch)");
                let results = score_all(&mut |q| {
                    store
                        .search_vec(q, 10, Some("eval-docs"))
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| r.file)
                        .collect()
                });
                print_report("Vector search", &VEC_THRESHOLDS, &results);
            } else {
                println!("\nMode: hybrid BM25 + vector + RRF + rerank");
                let results = score_all(&mut |q| {
                    store
                        .hybrid_query(q, 10, Some("eval-docs"), false)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|r| r.file)
                        .collect()
                });
                print_report(
                    "Hybrid query (BM25 + vec + RRF + rerank)",
                    &HYBRID_THRESHOLDS,
                    &results,
                );
            }
        }

        _ => {
            // Default: BM25 only — no model download required.
            println!("Mode: BM25 / FTS only (no model required)");
            let (_tmp, mut store) = make_temp_store(Box::new(rqmd_llm::NoBackend))?;

            println!("Indexing corpus ({} docs)...", EVAL_DOCS.len());
            index_all_fts_only(&mut store)?;
            println!("Indexing complete.");

            let results = score_all(&mut |q| {
                store
                    .search_fts(q, 10, Some("eval-docs"))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| r.file)
                    .collect()
            });

            if verbose {
                println!("\n  Per-query breakdown:");
                for tier in [
                    Difficulty::Easy,
                    Difficulty::Medium,
                    Difficulty::Hard,
                    Difficulty::Fusion,
                ] {
                    println!("  {:?}:", tier);
                    for q in EVAL_QUERIES.iter().filter(|q| q.difficulty == tier) {
                        let hits = store
                            .search_fts(q.query, 5, Some("eval-docs"))
                            .unwrap_or_default()
                            .into_iter()
                            .map(|r| r.file)
                            .collect::<Vec<_>>();
                        let found = hit_at_k(&hits, q.expected_doc, 5);
                        let mark = if found { "✓" } else { "✗" };
                        println!("    {mark} {:?}  {:?}", q.query, q.expected_doc);
                    }
                }
            }

            print_report("BM25 / FTS", &BM25_THRESHOLDS, &results);
        }
    }

    Ok(())
}

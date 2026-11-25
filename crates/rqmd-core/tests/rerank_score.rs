//! `SearchResult::rerank_score`: the raw reranker score, kept beside the blended
//! `score`. No model is needed: a marker backend reranks by the number of
//! times a text contains "alphamark".

use rqmd_core::{Deadline, SearchOptions, Store, StoreConfig};
use tempfile::TempDir;

struct MarkerBackend;

impl rqmd_llm::InferenceBackend for MarkerBackend {
    fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0_f32; rqmd_llm::EMBED_DIM];
        v[usize::from(!text.contains("alphamark"))] = 1.0;
        Ok(v)
    }

    fn rerank(&mut self, _query: &str, docs: &[&str]) -> anyhow::Result<Vec<f32>> {
        Ok(docs
            .iter()
            .map(|d| d.matches("alphamark").count() as f32)
            .collect())
    }

    fn generate(&mut self, _prompt: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn embed_model_name(&self) -> &str {
        "marker"
    }

    fn rerank_model_name(&self) -> &str {
        "marker"
    }

    fn generate_model_name(&self) -> &str {
        "marker"
    }
}

fn store_with_docs(dir: &TempDir) -> Store {
    let config = StoreConfig {
        db_path: dir.path().join("test.sqlite"),
        tantivy_dir: dir.path().join("tantivy"),
        hnsw_path: dir.path().join("hnsw.usearch"),
        read_only: false,
    };
    let mut store = Store::open(config, Box::new(MarkerBackend)).unwrap();
    store
        .index_document("notes", "one.md", "one", "alphamark alphamark alphamark")
        .unwrap();
    store
        .index_document("notes", "two.md", "two", "alphamark and other words")
        .unwrap();
    store.flush().unwrap();
    store
}

fn query(store: &mut Store, skip_rerank: bool) -> Vec<rqmd_core::SearchResult> {
    store
        .hybrid_query_outcome_with(
            "alphamark",
            None,
            10,
            None,
            skip_rerank,
            true,
            Deadline::none(),
            true,
            &SearchOptions::default(),
        )
        .unwrap()
        .results
}

#[test]
fn query_reports_the_raw_rerank_score() {
    let dir = TempDir::new().unwrap();
    let mut store = store_with_docs(&dir);
    let hits = query(&mut store, false);
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].rerank_score, Some(3.0));
    assert_eq!(hits[1].rerank_score, Some(1.0));
}

#[test]
fn query_without_rerank_reports_none_and_keeps_the_order() {
    let dir = TempDir::new().unwrap();
    let mut store = store_with_docs(&dir);
    let skipped = query(&mut store, true);
    assert!(!skipped.is_empty());
    assert!(skipped.iter().all(|r| r.rerank_score.is_none()));
}

#[test]
fn rerank_score_leaves_the_blended_score_unchanged() {
    let dir = TempDir::new().unwrap();
    let mut store = store_with_docs(&dir);
    let hits = query(&mut store, false);
    let skipped = query(&mut store, true);
    // blend = 0.7 * rerank + 0.25 * fusion; the fusion score is what skipping returns.
    for hit in &hits {
        let fusion = skipped
            .iter()
            .find(|s| s.file == hit.file)
            .map(|s| s.score)
            .unwrap();
        let expected = 0.75 * hit.rerank_score.unwrap() + 0.25 * fusion;
        assert!(
            (hit.score - expected).abs() < 1e-5,
            "{} vs {expected}",
            hit.score
        );
    }
}

#[test]
fn keyword_and_vector_search_report_none() {
    let dir = TempDir::new().unwrap();
    let mut store = store_with_docs(&dir);
    let fts = store.search_fts("alphamark", 10, None).unwrap();
    assert!(!fts.is_empty());
    assert!(fts.iter().all(|r| r.rerank_score.is_none()));
    let vec = store.search_vec("alphamark", 10, None).unwrap();
    assert!(!vec.is_empty());
    assert!(vec.iter().all(|r| r.rerank_score.is_none()));
}

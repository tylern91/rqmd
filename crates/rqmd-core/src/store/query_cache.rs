//! How the hybrid query uses the opt-in query cache ([`crate::model_cache`]).
//! Kept apart from the query pipeline in `store.rs`, which only calls the three
//! hooks below; without [`Store::enable_model_cache`] each hook is the plain
//! model call it wraps.

use anyhow::{Result, bail};
use std::path::Path;

use super::Store;
use crate::deadline::Deadline;
use crate::model_cache::{self, ModelCache};

/// `tracing` target of the cache's debug lines, which say per query stage how
/// much came from the cache and how much from the model
/// (`RUST_LOG=rqmd::query_cache=debug`). Only written while the cache is on.
const TRACE_TARGET: &str = "rqmd::query_cache";

impl Store {
    /// Reuse expansion output and rerank scores from earlier queries, stored in
    /// the cache file at `path` (normally [`model_cache::path_in`] of the index
    /// directory), and store new ones there. Off unless this is called. Fails,
    /// leaving the cache off, when the file cannot be created or read.
    pub fn enable_model_cache(&mut self, path: &Path) -> Result<()> {
        self.model_cache = Some(ModelCache::open(path)?);
        Ok(())
    }

    /// Whether [`Self::enable_model_cache`] succeeded and the cache has not been
    /// turned off since by an error.
    pub fn model_cache_enabled(&self) -> bool {
        self.model_cache.is_some()
    }

    /// A cache operation failed. Lock contention from another process passes on
    /// its own, so it only skips this operation; anything else turns the cache
    /// off for the rest of this store's life, with one warning. The query goes
    /// on with the model either way.
    fn cache_failed(&mut self, err: anyhow::Error) {
        if model_cache::is_busy(&err) {
            tracing::debug!(target: TRACE_TARGET, "query cache busy, skipped: {err:#}");
            return;
        }
        tracing::warn!(
            "query cache turned off for this run, results are computed instead: {err:#}"
        );
        self.model_cache = None;
    }

    /// Hook for query expansion: the generation model's output for `prompt`,
    /// from the cache when it holds one for the current model, otherwise from
    /// the model, which is then stored. A failed generation stores nothing.
    pub(super) fn generate_expansion(&mut self, prompt: &str) -> Result<String> {
        if self.model_cache.is_none() {
            return self.backend.generate(prompt);
        }
        let model = model_cache::model_identity(
            self.backend.generate_model_name(),
            self.backend.generate_model_file(),
        );
        if let (Some(cache), Some(model)) = (self.model_cache.as_mut(), model.as_deref()) {
            match cache.get_expansion(prompt, model) {
                Ok(Some(output)) => {
                    tracing::debug!(target: TRACE_TARGET, "expansion: from cache");
                    // Mark it recently used.
                    if let Err(e) = cache.put_expansion(prompt, model, &output) {
                        self.cache_failed(e);
                    }
                    return Ok(output);
                }
                Ok(None) => {}
                Err(e) => self.cache_failed(e),
            }
        }
        let output = self.backend.generate(prompt)?;
        tracing::debug!(target: TRACE_TARGET, "expansion: from model");
        if let (Some(cache), Some(model)) = (self.model_cache.as_mut(), model.as_deref())
            && let Err(e) = cache.put_expansion(prompt, model, &output)
        {
            self.cache_failed(e);
        }
        Ok(output)
    }

    /// Hook for reranking: [`Self::rerank_candidates`] that takes each chunk's
    /// score from the cache when it holds one for the current model and asks
    /// the model only for the rest. With no cached score this is exactly one
    /// [`Self::rerank_candidates`] call over every chunk. Reads only; scores are
    /// stored by [`Self::remember_rerank_scores`] once the rerank is known to
    /// have succeeded.
    pub(super) fn rerank_candidates_cached(
        &mut self,
        rerank_query: &str,
        chunks: &[&str],
        soft_deadline: Option<Deadline>,
    ) -> Result<Option<Vec<f32>>> {
        let Some(model) = self.rerank_identity() else {
            return self.rerank_candidates(rerank_query, chunks, soft_deadline);
        };
        let lookup = match self.model_cache.as_ref() {
            Some(cache) => cache.get_rerank_scores(rerank_query, &model, chunks),
            None => return self.rerank_candidates(rerank_query, chunks, soft_deadline),
        };
        let cached = match lookup {
            Ok(cached) => cached,
            Err(e) => {
                self.cache_failed(e);
                vec![None; chunks.len()]
            }
        };
        let misses: Vec<&str> = chunks
            .iter()
            .zip(&cached)
            .filter(|(_, score)| score.is_none())
            .map(|(chunk, _)| *chunk)
            .collect();
        tracing::debug!(
            target: TRACE_TARGET,
            "rerank: {} from cache, {} from model",
            chunks.len() - misses.len(),
            misses.len()
        );
        if misses.len() == chunks.len() {
            return self.rerank_candidates(rerank_query, chunks, soft_deadline);
        }
        let fresh = if misses.is_empty() {
            Vec::new()
        } else {
            match self.rerank_candidates(rerank_query, &misses, soft_deadline)? {
                Some(fresh) => fresh,
                None => return Ok(None),
            }
        };
        if fresh.len() != misses.len() {
            bail!(
                "rerank returned {} score(s) for {} candidate(s)",
                fresh.len(),
                misses.len()
            );
        }
        let mut fresh = fresh.into_iter();
        Ok(Some(
            cached
                .into_iter()
                .map(|score| score.or_else(|| fresh.next()).unwrap_or(f32::NAN))
                .collect(),
        ))
    }

    /// Hook after reranking: store `scores`, one per chunk, which the caller
    /// passes only for a rerank that finished with usable model scores.
    pub(super) fn remember_rerank_scores(
        &mut self,
        rerank_query: &str,
        chunks: &[&str],
        scores: &[f32],
    ) {
        let Some(model) = self.rerank_identity() else {
            return;
        };
        let scored: Vec<(&str, f32)> = chunks.iter().copied().zip(scores.iter().copied()).collect();
        if let Some(cache) = self.model_cache.as_mut()
            && let Err(e) = cache.put_rerank_scores(rerank_query, &model, &scored)
        {
            self.cache_failed(e);
        }
    }

    /// The rerank model's cache identity, or `None` when the cache is off or the
    /// model file cannot be read.
    fn rerank_identity(&self) -> Option<String> {
        self.model_cache.as_ref()?;
        model_cache::model_identity(
            self.backend.rerank_model_name(),
            self.backend.rerank_model_file(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QueryOutcome, RerankStatus, StoreConfig};
    use std::sync::{Arc, Mutex};

    /// How the stub reranker answers.
    #[derive(Clone, Copy, PartialEq)]
    enum Rerank {
        /// One score per chunk, from the chunk's length.
        Scores,
        Fail,
        /// One score too few.
        TooFew,
        AllNegInfinity,
    }

    #[derive(Default)]
    struct Calls {
        generate: usize,
        /// Chunks passed to each rerank call.
        rerank: Vec<usize>,
    }

    struct Stub {
        calls: Arc<Mutex<Calls>>,
        rerank: Arc<Mutex<Rerank>>,
        generate_fails: Arc<Mutex<bool>>,
        rerank_name: Arc<Mutex<String>>,
        generate_name: Arc<Mutex<String>>,
        slow_embed: Arc<Mutex<bool>>,
        can_rerank: Arc<Mutex<bool>>,
    }

    impl rqmd_llm::InferenceBackend for Stub {
        fn capabilities(&self) -> rqmd_llm::BackendCapabilities {
            rqmd_llm::BackendCapabilities {
                embed: true,
                rerank: *self.can_rerank.lock().unwrap(),
                generate: true,
            }
        }
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            if *self.slow_embed.lock().unwrap() {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            self.calls.lock().unwrap().rerank.push(docs.len());
            let scores: Vec<f32> = docs.iter().map(|d| d.len() as f32 / 100.0).collect();
            match *self.rerank.lock().unwrap() {
                Rerank::Scores => Ok(scores),
                Rerank::Fail => bail!("rerank model failed"),
                Rerank::TooFew => Ok(scores[1..].to_vec()),
                Rerank::AllNegInfinity => Ok(vec![f32::NEG_INFINITY; docs.len()]),
            }
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            self.calls.lock().unwrap().generate += 1;
            if *self.generate_fails.lock().unwrap() {
                bail!("generation failed");
            }
            Ok("lex: harbour\nvec: harbour boats\nhyde: boats in a harbour".to_string())
        }
        fn embed_model_name(&self) -> &str {
            "stub-embed"
        }
        fn rerank_model_name(&self) -> &str {
            // Leaked so the trait can hand out a `&str` of a name the test
            // changes; a handful of short strings per test.
            Box::leak(self.rerank_name.lock().unwrap().clone().into_boxed_str())
        }
        fn generate_model_name(&self) -> &str {
            Box::leak(self.generate_name.lock().unwrap().clone().into_boxed_str())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        cache_path: std::path::PathBuf,
        calls: Arc<Mutex<Calls>>,
        rerank: Arc<Mutex<Rerank>>,
        generate_fails: Arc<Mutex<bool>>,
        rerank_name: Arc<Mutex<String>>,
        generate_name: Arc<Mutex<String>>,
        slow_embed: Arc<Mutex<bool>>,
        can_rerank: Arc<Mutex<bool>>,
    }

    impl Fixture {
        /// Three notes that each mention `uniqueterm` once, so no keyword hit
        /// dominates and the query is expanded.
        fn new(cache: bool) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let calls = Arc::new(Mutex::new(Calls::default()));
            let rerank = Arc::new(Mutex::new(Rerank::Scores));
            let generate_fails = Arc::new(Mutex::new(false));
            let rerank_name = Arc::new(Mutex::new("rerank-a".to_string()));
            let generate_name = Arc::new(Mutex::new("generate-a".to_string()));
            let slow_embed = Arc::new(Mutex::new(false));
            let can_rerank = Arc::new(Mutex::new(true));
            let backend = Stub {
                can_rerank: Arc::clone(&can_rerank),
                slow_embed: Arc::clone(&slow_embed),
                calls: Arc::clone(&calls),
                rerank: Arc::clone(&rerank),
                generate_fails: Arc::clone(&generate_fails),
                rerank_name: Arc::clone(&rerank_name),
                generate_name: Arc::clone(&generate_name),
            };
            let config = StoreConfig {
                db_path: dir.path().join("index.sqlite"),
                tantivy_dir: dir.path().join("tantivy"),
                hnsw_path: dir.path().join("hnsw.usearch"),
                read_only: false,
            };
            let mut store = Store::open(config, Box::new(backend)).unwrap();
            for (path, body) in [
                ("a.md", "uniqueterm alpha one"),
                ("b.md", "uniqueterm beta two"),
                ("c.md", "uniqueterm gamma six"),
            ] {
                store.index_document("coll", path, "T", body).unwrap();
            }
            store.flush().unwrap();
            let cache_path = model_cache::path_in(dir.path());
            if cache {
                store.enable_model_cache(&cache_path).unwrap();
            }
            Self {
                _dir: dir,
                store,
                cache_path,
                calls,
                rerank,
                generate_fails,
                rerank_name,
                generate_name,
                slow_embed,
                can_rerank,
            }
        }

        fn query_with(&mut self, deadline: Deadline, no_expand: bool) -> QueryOutcome {
            self.store
                .hybrid_query_outcome(
                    "uniqueterm",
                    None,
                    10,
                    None,
                    false,
                    no_expand,
                    deadline,
                    false,
                )
                .unwrap()
        }

        fn query(&mut self) -> QueryOutcome {
            self.query_with(Deadline::none(), false)
        }

        fn take_calls(&self) -> (usize, Vec<usize>) {
            let mut calls = self.calls.lock().unwrap();
            let taken = (calls.generate, calls.rerank.clone());
            *calls = Calls::default();
            taken
        }

        fn rows(&self) -> u64 {
            model_cache::row_count_at(&self.cache_path)
                .unwrap()
                .unwrap_or(0)
        }
    }

    fn ranked(outcome: &QueryOutcome) -> Vec<(String, u32)> {
        outcome
            .results
            .iter()
            .map(|r| (r.file.clone(), r.score.to_bits()))
            .collect()
    }

    #[test]
    fn ac_1_a_repeated_query_calls_each_model_once_and_ranks_identically() {
        let mut fx = Fixture::new(true);
        let first = fx.query();
        assert_eq!(fx.take_calls(), (1, vec![3]));
        assert_eq!(first.rerank, RerankStatus::Reranked);
        assert_eq!(fx.rows(), 4, "one expansion and three rerank rows");

        let second = fx.query();

        assert_eq!(fx.take_calls(), (0, vec![]));
        assert_eq!(second.rerank, RerankStatus::Reranked);
        assert_eq!(ranked(&second), ranked(&first));
    }

    #[test]
    fn with_the_cache_off_nothing_is_stored_and_every_query_calls_the_models() {
        let mut fx = Fixture::new(false);
        let first = fx.query();
        let second = fx.query();
        assert_eq!(fx.take_calls(), (2, vec![3, 3]));
        assert_eq!(ranked(&second), ranked(&first));
        assert!(!fx.cache_path.exists());
    }

    #[test]
    fn a_cold_cache_ranks_exactly_like_no_cache() {
        let mut fx = Fixture::new(false);
        let off = fx.query();
        fx.store.enable_model_cache(&fx.cache_path).unwrap();
        let cold = fx.query();
        assert_eq!(fx.take_calls(), (2, vec![3, 3]));
        assert_eq!(ranked(&cold), ranked(&off));
    }

    #[test]
    fn ac_2_an_edited_chunk_is_rescored_and_the_others_are_reused() {
        let mut fx = Fixture::new(true);
        let first = fx.query();
        fx.take_calls();
        fx.store
            .index_document("coll", "b.md", "T", "uniqueterm beta two, edited")
            .unwrap();
        fx.store.flush().unwrap();

        let second = fx.query();

        assert_eq!(fx.take_calls(), (0, vec![1]), "only the edited chunk");
        assert_eq!(second.rerank, RerankStatus::Reranked);
        assert_ne!(ranked(&second), ranked(&first));
    }

    #[test]
    fn ac_3_another_model_name_misses_the_cache() {
        let mut fx = Fixture::new(true);
        fx.query();
        fx.take_calls();

        *fx.generate_name.lock().unwrap() = "generate-b".to_string();
        fx.query();
        assert_eq!(fx.take_calls(), (1, vec![]), "new generation model");

        *fx.rerank_name.lock().unwrap() = "rerank-b".to_string();
        fx.query();
        assert_eq!(fx.take_calls(), (0, vec![3]), "new rerank model");
    }

    #[test]
    fn ac_4_a_failed_or_unusable_rerank_stores_nothing() {
        for mode in [Rerank::Fail, Rerank::TooFew, Rerank::AllNegInfinity] {
            let mut fx = Fixture::new(true);
            *fx.rerank.lock().unwrap() = mode;
            assert_eq!(fx.query().rerank, RerankStatus::Failed);
            assert_eq!(fx.rows(), 1, "the expansion only");
        }
    }

    #[test]
    fn ac_4_a_rerank_that_ran_out_of_time_stores_nothing() {
        let mut fx = Fixture::new(true);
        // Embedding the query outlasts the deadline, which then passes before
        // reranking: the soft deadline returns fusion order.
        *fx.slow_embed.lock().unwrap() = true;
        let outcome = fx.query_with(Deadline::after(std::time::Duration::from_millis(100)), true);
        assert_eq!(outcome.rerank, RerankStatus::DeadlineExpired);
        assert_eq!(fx.rows(), 0);
    }

    #[test]
    fn ac_4_a_failed_expansion_stores_nothing() {
        let mut fx = Fixture::new(true);
        *fx.generate_fails.lock().unwrap() = true;
        *fx.rerank.lock().unwrap() = Rerank::Fail;
        fx.query();
        assert_eq!(fx.rows(), 0);
        assert_eq!(fx.take_calls().0, 1);

        fx.query();
        assert_eq!(
            fx.take_calls().0,
            1,
            "nothing was cached, so it is asked again"
        );
    }

    #[test]
    fn ac_4_a_partly_cached_rerank_that_fails_stores_nothing_new() {
        let mut fx = Fixture::new(true);
        fx.query();
        fx.store
            .index_document("coll", "c.md", "T", "uniqueterm gamma six, edited")
            .unwrap();
        fx.store.flush().unwrap();
        *fx.rerank.lock().unwrap() = Rerank::TooFew;

        assert_eq!(fx.query().rerank, RerankStatus::Failed);
        assert_eq!(fx.rows(), 4, "the rows of the first query only");
    }

    #[test]
    fn ac_3_another_prompt_version_misses_the_cache() {
        let mut fx = Fixture::new(true);
        fx.query();
        fx.take_calls();
        // Rows written under an earlier prompt version, as an upgrade leaves them.
        let conn = rusqlite::Connection::open(&fx.cache_path).unwrap();
        conn.execute_batch(
            "UPDATE expansion SET prompt_version = 'expansion-0';
             UPDATE rerank SET prompt_version = 'rerank-0';",
        )
        .unwrap();

        fx.query();
        assert_eq!(fx.take_calls(), (1, vec![3]));
        fx.query();
        assert_eq!(
            fx.take_calls(),
            (0, vec![]),
            "rewritten under the current version"
        );
    }

    #[test]
    fn ac_4_a_skipped_or_unsupported_rerank_stores_nothing() {
        let mut fx = Fixture::new(true);
        let outcome = fx
            .store
            .hybrid_query_outcome(
                "uniqueterm",
                None,
                10,
                None,
                true,
                true,
                Deadline::none(),
                false,
            )
            .unwrap();
        assert_eq!(outcome.rerank, RerankStatus::NotRun);
        assert_eq!(fx.take_calls(), (0, vec![]));

        *fx.can_rerank.lock().unwrap() = false;
        assert_eq!(
            fx.query_with(Deadline::none(), true).rerank,
            RerankStatus::Unsupported
        );
        assert_eq!(fx.rows(), 0);
    }

    #[test]
    fn ac_5_a_full_cache_stays_at_its_limit_and_keeps_the_newest_rows() {
        let mut fx = Fixture::new(true);
        let filler: Vec<String> = (0..model_cache::MAX_ROWS)
            .map(|i| format!("filler {i}"))
            .collect();
        let scored: Vec<(&str, f32)> = filler.iter().map(|c| (c.as_str(), 0.5)).collect();
        ModelCache::open(&fx.cache_path)
            .unwrap()
            .put_rerank_scores("filler query", "filler-model", &scored)
            .unwrap();

        fx.query();
        assert_eq!(fx.rows(), model_cache::MAX_ROWS);
        fx.take_calls();
        fx.query();
        assert_eq!(fx.take_calls(), (0, vec![]), "this query's rows survived");
    }
}

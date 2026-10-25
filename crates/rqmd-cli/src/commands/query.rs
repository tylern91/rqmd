use anyhow::Result;
use rqmd_core::{
    Deadline, QueryOutcome, QueryStage, Recency, RerankStatus, SearchOptions, SearchResult, Store,
};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::format::{Format, Progress, ProgressMode};
use crate::{format, store};

/// Open a store, run one `search` step against it, and print the results —
/// the shape shared by `run_query`, `run_search`, and `run_vsearch`. Only the
/// store-opening mode (backend needed or not, fingerprint staleness worth
/// warning about) and the search step itself differ between them.
fn run_search_command(
    index_dir: &Path,
    query: &str,
    fmt: Format,
    full: bool,
    with_backend: bool,
    search: impl FnOnce(&mut Store) -> Result<Vec<SearchResult>>,
) -> Result<()> {
    let mut s = if with_backend {
        let s = store::open_store_with_backend(index_dir, true)?;
        store::warn_if_fingerprint_stale(&s);
        s
    } else {
        store::open_store_no_backend(index_dir, true)?
    };
    store::warn_if_raw_backfill_pending(&s);
    store::warn_if_fts_rebuild_pending(&s);
    let results = search(&mut s)?;
    let roots = store::collection_roots(&s, fmt)?;
    format::print_results(&results, fmt, full, query, &roots);
    Ok(())
}

/// Options for `run_query` beyond the store location and the query text
/// itself — bundled so the function stays under clippy's argument-count
/// lint instead of carrying all nine as positional parameters.
pub struct QueryOptions<'a> {
    pub intent: Option<&'a str>,
    pub collections: Option<&'a [String]>,
    pub num: usize,
    /// Sections of one file to return; the library caps it at 5.
    pub per_file: usize,
    pub fmt: Format,
    pub no_rerank: bool,
    pub full: bool,
    pub no_expand: bool,
    pub timeout: Option<Duration>,
    /// Fail with exit 124 when `timeout` passes before reranking, instead of
    /// returning the results in fusion order.
    pub timeout_strict: bool,
    /// Weight scores by file age.
    pub recency: Option<Recency>,
}

/// Stderr line for a query whose results were not reranked although reranking
/// was expected. Plain text, so it reads the same under `NO_COLOR` and
/// `TERM=dumb`, and stdout stays machine-parseable.
fn rerank_warning(status: RerankStatus, timeout: Option<Duration>) -> Option<String> {
    let notice = status.notice()?;
    let hint = match (status, timeout) {
        (RerankStatus::Failed, _) => " (set RUST_LOG=warn for details)".to_string(),
        (RerankStatus::DeadlineExpired, Some(limit)) => format!(
            " (--timeout of {}s; add --timeout-strict to exit 124 instead)",
            limit.as_secs()
        ),
        _ => String::new(),
    };
    Some(format!("rqmd: warning: {notice}{hint}"))
}

/// Stderr text for the stage a query has reached. The first stage that needs
/// a model includes loading it, so its label says so.
fn stage_label(stage: QueryStage) -> String {
    match stage {
        QueryStage::Searching => "Searching keywords".to_string(),
        QueryStage::Embedding => "Loading model / embedding query".to_string(),
        QueryStage::Expanding => "Loading model / expanding query".to_string(),
        QueryStage::Reranking { done: 0, total } => {
            format!("Loading model / reranking 0/{total}")
        }
        QueryStage::Reranking { done, total } => format!("Reranking {done}/{total}"),
    }
}

/// A query observer that shows each stage through `progress`, once per change.
fn stage_reporter<W: Write + Send + 'static>(
    progress: Arc<Mutex<Progress<W>>>,
) -> Box<dyn FnMut(QueryStage) + Send> {
    let mut last = None;
    Box::new(move |stage| {
        if last.replace(stage) == Some(stage) {
            return;
        }
        let mut progress = progress.lock().unwrap_or_else(PoisonError::into_inner);
        let line = stage_label(stage);
        match stage {
            QueryStage::Reranking { done, total } => progress.show(&line, done, total),
            _ => progress.stage(&line),
        }
    })
}

/// Run the hybrid query, showing its stages through `progress` and wiping the
/// line before the outcome (or the error) is reported. An inactive `progress`
/// installs no observer, so the query runs exactly as it does without one.
fn query_with_progress<W: Write + Send + 'static>(
    s: &mut Store,
    query: &str,
    opts: &QueryOptions,
    progress: Progress<W>,
) -> Result<QueryOutcome> {
    // Starts once the store is open, so index load is not charged; models load
    // lazily inside the stages and count against it.
    let deadline = opts.timeout.map_or(Deadline::none(), Deadline::after);
    let active = progress.is_active();
    let progress = Arc::new(Mutex::new(progress));
    if active {
        s.set_query_observer(Some(stage_reporter(Arc::clone(&progress))));
    }
    let outcome = s.hybrid_query_outcome_with(
        query,
        opts.intent,
        opts.num,
        opts.collections,
        opts.no_rerank,
        opts.no_expand,
        deadline,
        opts.timeout_strict,
        &SearchOptions {
            per_file: opts.per_file,
            recency: opts.recency,
        },
    );
    s.set_query_observer(None);
    progress
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    outcome
}

pub fn run_query(index_dir: &Path, query: &str, opts: QueryOptions) -> Result<()> {
    run_search_command(index_dir, query, opts.fmt, opts.full, true, |s| {
        if let Some(recency) = &opts.recency {
            eprintln!("rqmd: {}", recency.notice());
        }
        let progress = Progress::new(ProgressMode::detect());
        let outcome = query_with_progress(s, query, &opts, progress)?;
        if let Some(line) = rerank_warning(outcome.rerank, opts.timeout) {
            eprintln!("{line}");
        }
        Ok(outcome.results)
    })
}

/// Say on stderr that results are weighted by file age, as `query` does.
fn announce_recency(options: &SearchOptions) {
    if let Some(recency) = &options.recency {
        eprintln!("rqmd: {}", recency.notice());
    }
}

pub fn run_search(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    options: SearchOptions,
    fmt: Format,
    full: bool,
) -> Result<()> {
    run_search_command(index_dir, query, fmt, full, false, |s| {
        announce_recency(&options);
        s.search_fts_multi_with(query, num, collections, &options)
    })
}

pub fn run_vsearch(
    index_dir: &Path,
    query: &str,
    collections: Option<&[String]>,
    num: usize,
    options: SearchOptions,
    fmt: Format,
    full: bool,
) -> Result<()> {
    run_search_command(index_dir, query, fmt, full, true, |s| {
        announce_recency(&options);
        s.search_vec_multi_with(query, num, collections, &options)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn rerank_warning_is_one_plain_line_only_for_degraded_runs() {
        let limit = Some(Duration::from_secs(7));
        assert_eq!(rerank_warning(RerankStatus::Reranked, limit), None);
        assert_eq!(rerank_warning(RerankStatus::NotRun, limit), None);
        for status in [
            RerankStatus::Unsupported,
            RerankStatus::Failed,
            RerankStatus::DeadlineExpired,
        ] {
            let line = rerank_warning(status, limit).expect("warning");
            assert!(line.starts_with("rqmd: warning: results were not reranked"));
            assert!(!line.contains('\n') && !line.contains('\x1b'), "{line:?}");
        }
    }

    #[test]
    fn deadline_warning_names_the_timeout_and_the_strict_flag() {
        let line = rerank_warning(RerankStatus::DeadlineExpired, Some(Duration::from_secs(7)))
            .expect("warning");
        assert!(line.contains("--timeout of 7s"), "{line}");
        assert!(line.contains("--timeout-strict"), "{line}");
    }

    /// A writer the test can read back after the progress display owns it.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// Scores a candidate from its own text and counts the candidates in each
    /// reranker call. Once `fail_embed` is set, embedding fails: the query stops
    /// at its first model use.
    struct CountingBackend {
        rerank_calls: Arc<Mutex<Vec<usize>>>,
        fail_embed: Arc<AtomicBool>,
    }

    impl rqmd_llm::InferenceBackend for CountingBackend {
        fn embed(&mut self, _text: &str) -> Result<Vec<f32>> {
            anyhow::ensure!(
                !self.fail_embed.load(Ordering::SeqCst),
                "embed model unavailable"
            );
            Ok(vec![0.1; rqmd_llm::EMBED_DIM])
        }
        fn rerank(&mut self, _query: &str, docs: &[&str]) -> Result<Vec<f32>> {
            self.rerank_calls.lock().unwrap().push(docs.len());
            Ok(docs.iter().map(|d| d.len() as f32 / 100.0).collect())
        }
        fn generate(&mut self, _prompt: &str) -> Result<String> {
            Ok(String::new())
        }
        fn embed_model_name(&self) -> &str {
            "counting"
        }
        fn rerank_model_name(&self) -> &str {
            "counting"
        }
        fn generate_model_name(&self) -> &str {
            "counting"
        }
    }

    /// An index of nine documents matching `uniquerankterm`.
    fn nine_doc_store(dir: &Path) -> (Store, Arc<Mutex<Vec<usize>>>, Arc<AtomicBool>) {
        let rerank_calls = Arc::new(Mutex::new(Vec::new()));
        let fail_embed = Arc::new(AtomicBool::new(false));
        let backend = CountingBackend {
            rerank_calls: Arc::clone(&rerank_calls),
            fail_embed: Arc::clone(&fail_embed),
        };
        let mut s = Store::open(store::store_config(dir, false), Box::new(backend)).unwrap();
        for i in 0..9 {
            let body = format!("uniquerankterm {}", "filler ".repeat(i + 1));
            s.index_document("coll", &format!("d{i}.md"), "T", &body)
                .unwrap();
        }
        s.flush().unwrap();
        (s, rerank_calls, fail_embed)
    }

    fn options() -> QueryOptions<'static> {
        QueryOptions {
            intent: None,
            collections: None,
            num: 10,
            per_file: 1,
            fmt: Format::Cli,
            no_rerank: false,
            full: false,
            no_expand: true,
            timeout: None,
            timeout_strict: false,
            recency: None,
        }
    }

    fn run(s: &mut Store, mode: ProgressMode) -> (Result<QueryOutcome>, String) {
        run_query_text(s, "uniquerankterm", mode)
    }

    fn run_query_text(
        s: &mut Store,
        query: &str,
        mode: ProgressMode,
    ) -> (Result<QueryOutcome>, String) {
        let out = Captured::default();
        let progress = Progress::with_writer(mode, out.clone());
        let outcome = query_with_progress(s, query, &options(), progress);
        (outcome, out.text())
    }

    fn ranked(outcome: &QueryOutcome) -> Vec<(String, u32)> {
        outcome
            .results
            .iter()
            .map(|r| (r.file.clone(), r.score.to_bits()))
            .collect()
    }

    #[test]
    fn stage_labels_name_the_model_load_with_the_stage_that_needs_it() {
        assert_eq!(stage_label(QueryStage::Searching), "Searching keywords");
        assert_eq!(
            stage_label(QueryStage::Embedding),
            "Loading model / embedding query"
        );
        assert_eq!(
            stage_label(QueryStage::Expanding),
            "Loading model / expanding query"
        );
        let rerank = |done, total| stage_label(QueryStage::Reranking { done, total });
        assert_eq!(rerank(0, 9), "Loading model / reranking 0/9");
        assert_eq!(rerank(4, 9), "Reranking 4/9");
    }

    #[test]
    fn a_dumb_terminal_gets_one_plain_line_per_stage() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, ..) = nine_doc_store(dir.path());

        let (outcome, shown) = run(&mut s, ProgressMode::Lines);

        assert_eq!(outcome.unwrap().rerank, RerankStatus::Reranked);
        assert_eq!(
            shown,
            "Searching keywords\n\
             Loading model / embedding query\n\
             Loading model / reranking 0/9\n\
             Reranking 4/9\n\
             Reranking 8/9\n"
        );
    }

    #[test]
    fn a_terminal_redraws_one_line_and_wipes_it_afterwards() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, ..) = nine_doc_store(dir.path());

        let (outcome, shown) = run(&mut s, ProgressMode::Redraw);

        outcome.unwrap();
        assert!(!shown.contains('\n'), "{shown:?}");
        assert!(!shown.contains('\x1b'), "{shown:?}");
        assert!(shown.contains("\rSearching keywords"), "{shown:?}");
        assert!(shown.contains("\rReranking 8/9"), "{shown:?}");
        let wipe = shown.rsplit_once('\r').map(|(_, tail)| tail);
        assert_eq!(
            wipe,
            Some(""),
            "the last write returns the cursor: {shown:?}"
        );
        assert!(shown.trim_end_matches('\r').ends_with("  "), "{shown:?}");
    }

    /// Not a terminal: nothing is written and the reranker is called once for
    /// every candidate, as before the stages were reported.
    #[test]
    fn without_a_terminal_nothing_is_written_and_rerank_is_one_call() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, calls, _) = nine_doc_store(dir.path());

        let (outcome, shown) = run(&mut s, ProgressMode::Off);

        outcome.unwrap();
        assert_eq!(shown, "");
        assert_eq!(*calls.lock().unwrap(), [9]);
    }

    /// The same files, in the same order, with the same scores to the bit,
    /// whether or not the stages are shown.
    #[test]
    fn showing_stages_does_not_change_the_ranking() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, calls, _) = nine_doc_store(dir.path());

        let plain = run(&mut s, ProgressMode::Off).0.unwrap();
        let shown = run(&mut s, ProgressMode::Lines).0.unwrap();

        assert_eq!(*calls.lock().unwrap(), [9, 4, 4, 1]);
        assert_eq!(plain.results.len(), 9);
        assert_eq!(ranked(&shown), ranked(&plain));
    }

    /// A query that stops at its first model use still wipes the redrawn line
    /// before the error is reported.
    #[test]
    fn a_failed_query_wipes_the_stage_line_before_the_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, _, fail_embed) = nine_doc_store(dir.path());
        fail_embed.store(true, Ordering::SeqCst);

        let (outcome, shown) = run(&mut s, ProgressMode::Redraw);

        let err = outcome.unwrap_err();
        assert!(
            format!("{err:#}").contains("embed model unavailable"),
            "{err:#}"
        );
        assert!(
            shown.contains("Loading model / embedding query"),
            "{shown:?}"
        );
        assert!(shown.ends_with('\r'), "{shown:?}");
    }

    /// Consecutive sub-queries of one kind show their stage once.
    #[test]
    fn a_repeated_stage_is_shown_once() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, ..) = nine_doc_store(dir.path());

        let (outcome, shown) = run_query_text(
            &mut s,
            "vec: uniquerankterm\nhyde: uniquerankterm filler",
            ProgressMode::Lines,
        );

        outcome.unwrap();
        assert_eq!(shown.matches("Loading model / embedding query").count(), 1);
        assert!(
            shown.starts_with("Loading model / embedding query\n"),
            "{shown:?}"
        );
    }
}

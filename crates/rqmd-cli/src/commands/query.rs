use anyhow::Result;
use rqmd_core::{
    Deadline, QueryOutcome, QueryStage, QueryTimings, Recency, RerankStatus, SearchOptions,
    SearchResult, Store,
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
        let s = store::open_existing_store_with_backend(index_dir, true)?;
        store::warn_if_fingerprint_stale(&s);
        s
    } else {
        store::open_existing_store_no_backend(index_dir, true)?
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
    /// After the results, print per-stage durations and counts to stderr.
    pub timings: bool,
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

/// Milliseconds with two decimals, the unit every timings line uses.
fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

/// The `--timings` report: one `rqmd: timings:` line per stage, for stderr.
/// Counts sit beside the durations they explain.
fn render_timings(t: &QueryTimings) -> String {
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let mut out = vec![format!("total {}", ms(t.total))];
    out.push(format!(
        "bm25 {} ({}, scope ids {})",
        ms(t.bm25),
        plural(t.bm25_calls, "call"),
        ms(t.bm25_scope)
    ));
    out.push(format!(
        "embed {} ({}, includes first model load)",
        ms(t.embed),
        plural(t.embed_calls, "call")
    ));
    for (i, p) in t.ann_passes.iter().enumerate() {
        out.push(format!(
            "ann pass {} [{}] k={} vectors fetched={} in scope={} ann={} per-hit lookups={} lookup time={}",
            i + 1,
            p.phase,
            p.k,
            p.fetched,
            p.in_scope,
            ms(p.ann),
            p.fetched,
            ms(p.lookup)
        ));
    }
    out.push(format!(
        "ann total {} pass(es) ann={} per-hit lookups={} lookup time={}",
        t.ann_passes.len(),
        ms(t.ann_time()),
        t.lookups(),
        ms(t.lookup_time())
    ));
    out.push(format!(
        "expansion generate {} search {} (search includes its embeds, ann passes and keyword calls above)",
        ms(t.expansion_generate),
        ms(t.expansion_search)
    ));
    out.push(format!(
        "fusion {} ({} fused into {})",
        ms(t.fuse),
        plural(t.fused_from, "list"),
        plural(t.fused, "candidate")
    ));
    out.push(format!(
        "resolve candidates {} ({})",
        ms(t.resolve),
        plural(t.resolve_lookups, "lookup")
    ));
    out.push(format!("chunk select {}", ms(t.chunk_select)));
    if t.rerank_chunks == 0 {
        out.push("rerank not run".to_string());
    } else {
        out.push(format!(
            "rerank {} ({})",
            ms(t.rerank),
            plural(t.rerank_chunks, "section")
        ));
    }
    out.push(format!("assemble {}", ms(t.assemble)));
    // Expansion search is a span around other stages, so it is left out here.
    let attributed = t.bm25
        + t.embed
        + t.ann_time()
        + t.lookup_time()
        + t.expansion_generate
        + t.fuse
        + t.resolve
        + t.chunk_select
        + t.rerank
        + t.assemble;
    out.push(format!(
        "unattributed {}",
        ms(t.total.saturating_sub(attributed))
    ));
    out.iter()
        .map(|line| format!("rqmd: timings: {line}\n"))
        .collect()
}

pub fn run_query(index_dir: &Path, query: &str, opts: QueryOptions) -> Result<()> {
    let mut timings = None;
    run_search_command(index_dir, query, opts.fmt, opts.full, true, |s| {
        if let Some(recency) = &opts.recency {
            eprintln!("rqmd: {}", recency.notice());
        }
        s.set_query_timings(opts.timings);
        let progress = Progress::new(ProgressMode::detect());
        let outcome = query_with_progress(s, query, &opts, progress)?;
        timings = s.take_query_timings();
        if let Some(line) = rerank_warning(outcome.rerank, opts.timeout) {
            eprintln!("{line}");
        }
        Ok(outcome.results)
    })?;
    // After the results, so a terminal shows the report last.
    if let Some(timings) = timings {
        eprint!("{}", render_timings(&timings));
    }
    Ok(())
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
    fn timings_report_is_plain_prefixed_lines_with_counts() {
        let mut t = QueryTimings {
            total: Duration::from_millis(10),
            bm25_calls: 1,
            fused: 3,
            fused_from: 2,
            ..QueryTimings::default()
        };
        t.ann_passes.push(rqmd_core::AnnPass {
            phase: "query",
            k: 20,
            fetched: 20,
            in_scope: 2,
            ann: Duration::from_millis(1),
            lookup: Duration::from_millis(2),
        });
        let report = render_timings(&t);
        assert!(report.lines().all(|l| l.starts_with("rqmd: timings: ")));
        assert!(!report.contains('\x1b'), "{report:?}");
        assert!(report.contains("ann pass 1 [query] k=20 vectors fetched=20 in scope=2"));
        assert!(report.contains("per-hit lookups=20 lookup time=2.00 ms"));
        assert!(report.contains("rerank not run"));
        assert!(report.contains("2 lists fused into 3 candidates"));
        assert!(report.contains("unattributed 7.00 ms"), "{report}");
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
            timings: false,
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

    /// rqmd#152, `--timings` as `run_query` drives it: the results are those
    /// of an untimed query, and the report names each stage that ran, with
    /// the counts beside it.
    #[test]
    fn timings_leave_the_results_alone_and_report_every_stage() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut s, ..) = nine_doc_store(dir.path());
        let (plain, _) = run(&mut s, ProgressMode::Off);
        let plain = plain.unwrap();
        assert!(s.take_query_timings().is_none(), "off unless asked for");

        s.set_query_timings(true);
        let (timed, _) = run(&mut s, ProgressMode::Off);
        let timed = timed.unwrap();
        let report = render_timings(&s.take_query_timings().expect("timings were on"));

        assert_eq!(ranked(&plain), ranked(&timed));
        assert_eq!(plain.rerank, timed.rerank);
        for stage in [
            "total ",
            "bm25 ",
            "(1 call, scope ids ",
            "embed ",
            "ann pass 1 [query] k=20 vectors fetched=9 in scope=9",
            "per-hit lookups=9",
            "fusion ",
            "2 lists fused into 9 candidates",
            "resolve candidates ",
            "(9 lookups)",
            "chunk select ",
            "(9 sections)",
            "assemble ",
            "unattributed ",
        ] {
            assert!(report.contains(stage), "no `{stage}` in:\n{report}");
        }
        assert!(!report.contains("rerank not run"), "{report}");
        assert!(report.lines().all(|l| l.starts_with("rqmd: timings: ")));
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

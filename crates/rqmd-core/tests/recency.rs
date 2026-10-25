//! Opt-in recency weighting of `query` results: `SearchOptions::recency`
//! multiplies a result's score by a decay of its file's mtime, never below a
//! floor. No model is needed: a marker backend embeds and reranks by the
//! marker word a text contains.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rqmd_core::{
    DEFAULT_RECENCY_FLOOR, Deadline, MAX_HALF_LIFE, Recency, SearchOptions, SearchResult, Store,
    StoreConfig, db, parse_half_life,
};
use tempfile::TempDir;

const DAY: u64 = 86_400;

/// Embeds and reranks every text containing "alphamark" identically.
struct MarkerBackend;

impl rqmd_llm::InferenceBackend for MarkerBackend {
    fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0_f32; rqmd_llm::EMBED_DIM];
        v[if text.contains("alphamark") { 1 } else { 3 }] = 1.0;
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

fn open_store(dir: &TempDir) -> Store {
    let config = StoreConfig {
        db_path: dir.path().join("test.sqlite"),
        tantivy_dir: dir.path().join("tantivy"),
        hnsw_path: dir.path().join("hnsw.usearch"),
        read_only: false,
    };
    Store::open(config, Box::new(MarkerBackend)).unwrap()
}

/// Index `path` with a body of the same length for every `tag`, and record
/// that it was last modified `age_days` ago (`None`: no recorded mtime).
fn index(store: &mut Store, path: &str, tag: &str, age_days: Option<u64>) {
    let body = format!("alphamark note {tag}\n");
    store.index_document("notes", path, path, &body).unwrap();
    let stat = age_days.map(|days| {
        let at = SystemTime::now() - Duration::from_secs(days * DAY);
        let ns = at.duration_since(UNIX_EPOCH).unwrap().as_nanos();
        (body.len() as i64, ns as i64)
    });
    db::set_document_stat(&store.db, "notes", path, stat).unwrap();
}

fn query(store: &mut Store, recency: Option<Recency>) -> Vec<SearchResult> {
    store
        .hybrid_query_outcome_with(
            "alphamark",
            None,
            10,
            None,
            false,
            true,
            Deadline::none(),
            true,
            &SearchOptions {
                recency,
                ..SearchOptions::default()
            },
        )
        .unwrap()
        .results
}

fn paths(results: &[SearchResult]) -> Vec<&str> {
    results.iter().map(|r| r.path.as_str()).collect()
}

fn thirty_days() -> Recency {
    Recency::new(parse_half_life("30d").unwrap())
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

/// With the flag absent the options are the old ones, and the file mtimes
/// recorded in the index have no effect on order or score.
#[test]
fn ac_1_without_recency_results_ignore_file_age() {
    assert_eq!(SearchOptions::default().recency, None);
    let dir = TempDir::new().unwrap();
    let mut store = open_store(&dir);
    index(&mut store, "a.md", "one", Some(900));
    index(&mut store, "b.md", "two", Some(1));
    index(&mut store, "c.md", "six", None);
    store.flush().unwrap();

    let before = store
        .hybrid_query_outcome(
            "alphamark",
            None,
            10,
            None,
            false,
            true,
            Deadline::none(),
            true,
        )
        .unwrap()
        .results;
    let with = query(&mut store, None);
    for path in ["a.md", "b.md", "c.md"] {
        db::set_document_stat(&store.db, "notes", path, None).unwrap();
    }
    let unstamped = query(&mut store, None);

    assert_eq!(before.len(), 3);
    for other in [&with, &unstamped] {
        assert_eq!(paths(other), paths(&before));
        for (x, y) in before.iter().zip(other.iter()) {
            assert_eq!(x.score, y.score, "{}", x.path);
        }
    }
}

/// The part of the score above the floor halves every half-life; the floor
/// is what a very old file keeps.
#[test]
fn ac_2_factor_halves_the_excess_over_the_floor_each_half_life() {
    let r = thirty_days();
    assert_eq!(DEFAULT_RECENCY_FLOOR, 0.5);
    assert!(close(r.factor(Duration::ZERO), 1.0));
    assert!(close(r.factor(Duration::from_secs(30 * DAY)), 0.75));
    assert!(close(r.factor(Duration::from_secs(60 * DAY)), 0.625));
    assert!(close(r.factor(Duration::from_secs(90 * DAY)), 0.5625));
    assert!(close(r.factor(Duration::from_secs(36_500 * DAY)), 0.5));
    assert!(close(r.factor(Duration::MAX), 0.5));

    let flat = Recency::new(Duration::from_secs(DAY)).with_floor(0.0);
    assert!(close(flat.factor(Duration::from_secs(DAY)), 0.5));
    assert!(close(flat.factor(Duration::from_secs(3 * DAY)), 0.125));

    let mut last = 1.0;
    for days in 0..400 {
        let f = r.factor(Duration::from_secs(days * DAY));
        assert!(f <= last && f >= 0.5, "day {days}: {f}");
        last = f;
    }
}

/// The notice is one plain line naming the half-life and the floor.
#[test]
fn ac_3_notice_names_half_life_and_floor_in_one_plain_line() {
    let notice = thirty_days().notice();
    assert!(notice.contains("half-life 30d"), "{notice}");
    assert!(notice.contains("below 50% of its score"), "{notice}");
    assert!(
        !notice.contains('\n') && !notice.contains('\x1b'),
        "{notice:?}"
    );
    let hours = Recency::new(parse_half_life("12h").unwrap()).notice();
    assert!(hours.contains("half-life 12h"), "{hours}");
}

/// A floor outside 0..=1, or not a number, never raises or flips a score.
#[test]
fn ac_8_floor_is_kept_within_zero_and_one() {
    let hl = Duration::from_secs(DAY);
    let age = Duration::from_secs(50 * DAY);
    assert!(close(Recency::new(hl).with_floor(7.0).factor(age), 1.0));
    assert!(close(Recency::new(hl).with_floor(-3.0).factor(age), 0.0));
    assert!(close(
        Recency::new(hl).with_floor(f32::NAN).factor(age),
        DEFAULT_RECENCY_FLOOR
    ));
}

/// Half-lives are whole numbers of days, weeks or hours; zero, negative,
/// unitless, fractional and overlong values are refused with an example.
#[test]
fn ac_4_half_life_parses_days_weeks_and_hours_and_refuses_the_rest() {
    assert_eq!(
        parse_half_life("30d").unwrap(),
        Duration::from_secs(30 * DAY)
    );
    assert_eq!(
        parse_half_life("2w").unwrap(),
        Duration::from_secs(14 * DAY)
    );
    assert_eq!(
        parse_half_life("12h").unwrap(),
        Duration::from_secs(12 * 3600)
    );
    assert_eq!(parse_half_life(" 1d ").unwrap(), Duration::from_secs(DAY));
    assert_eq!(parse_half_life("3650d").unwrap(), MAX_HALF_LIFE);

    for bad in [
        "",
        "d",
        "0d",
        "0h",
        "-5d",
        "30",
        "1.5d",
        "30D",
        "30m",
        "3651d",
        "522w",
        "99999999999999999999d",
        "30 d",
        "1d2h",
    ] {
        let err = parse_half_life(bad).unwrap_err();
        assert!(err.contains("30d, 2w or 12h"), "{bad:?}: {err}");
    }
}

/// Two files equally relevant: the one modified later ranks first, whichever
/// of the two it is.
#[test]
fn ac_5_the_newer_of_two_equally_relevant_files_ranks_first() {
    for (a_days, b_days, first) in [(400, 2, "b.md"), (2, 400, "a.md")] {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir);
        index(&mut store, "a.md", "one", Some(a_days));
        index(&mut store, "b.md", "two", Some(b_days));
        store.flush().unwrap();

        let weighted = query(&mut store, Some(thirty_days()));

        assert_eq!(weighted.len(), 2);
        assert_eq!(weighted[0].path, first, "a {a_days}d, b {b_days}d");
    }
}

/// An old file is down-weighted by at most the floor and is never dropped.
#[test]
fn ac_6_an_old_file_keeps_at_least_the_floor_share_of_its_score() {
    let dir = TempDir::new().unwrap();
    let mut store = open_store(&dir);
    index(&mut store, "old.md", "one", Some(3000));
    store.flush().unwrap();
    let plain = query(&mut store, None);
    let weighted = query(&mut store, Some(thirty_days()));

    assert_eq!(paths(&weighted), ["old.md"]);
    assert!(plain[0].score > 0.0);
    assert!(
        close(weighted[0].score, plain[0].score * 0.5),
        "{} vs {}",
        weighted[0].score,
        plain[0].score
    );

    let none = Recency::new(thirty_days().half_life).with_floor(1.0);
    assert!(close(
        query(&mut store, Some(none))[0].score,
        plain[0].score
    ));
}

/// Each file's score is its unweighted score times the factor of its age. A
/// file with no recorded mtime counts as new, a future mtime is not penalised,
/// and repeated runs give the same order.
#[test]
fn ac_7_scores_scale_by_age_missing_mtime_is_new_and_runs_repeat() {
    let dir = TempDir::new().unwrap();
    let mut store = open_store(&dir);
    index(&mut store, "a.md", "one", Some(10));
    index(&mut store, "c.md", "two", Some(100));
    index(&mut store, "m.md", "six", None);
    index(&mut store, "x.md", "ten", Some(10));
    store.flush().unwrap();
    let future = (SystemTime::now() + Duration::from_secs(5 * DAY))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    db::set_document_stat(&store.db, "notes", "x.md", Some((1, future))).unwrap();

    let plain = query(&mut store, None);
    let weighted = query(&mut store, Some(thirty_days()));
    let ratio = |path: &str| {
        let score = |rs: &[SearchResult]| rs.iter().find(|r| r.path == path).unwrap().score;
        score(&weighted) / score(&plain)
    };
    let r = thirty_days();

    assert!(close(
        ratio("a.md"),
        r.factor(Duration::from_secs(10 * DAY))
    ));
    assert!(close(
        ratio("c.md"),
        r.factor(Duration::from_secs(100 * DAY))
    ));
    assert!(close(ratio("m.md"), 1.0));
    assert!(close(ratio("x.md"), 1.0));
    for _ in 0..3 {
        assert_eq!(
            paths(&query(&mut store, Some(thirty_days()))),
            paths(&weighted)
        );
    }
}

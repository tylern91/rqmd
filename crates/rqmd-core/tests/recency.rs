//! Opt-in recency weighting of `query`, `search` and `vsearch` results:
//! `SearchOptions::recency` multiplies a result's score by a decay of its
//! file's mtime, never below a floor. No model is needed: a marker backend
//! embeds and reranks by the marker word a text contains.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rqmd_core::chunking::chunk_document;
use rqmd_core::{
    DEFAULT_RECENCY_FLOOR, Deadline, MAX_HALF_LIFE, Recency, SearchOptions, SearchResult, Store,
    StoreConfig, db, parse_half_life,
};
use tempfile::TempDir;

const DAY: u64 = 86_400;

/// Embeds and reranks every text containing "alphamark" identically. A text
/// with "halfmark" is 0.6 similar to an "alphamark" query.
struct MarkerBackend;

impl rqmd_llm::InferenceBackend for MarkerBackend {
    fn embed(&mut self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0_f32; rqmd_llm::EMBED_DIM];
        if text.contains("alphamark") {
            v[1] = 1.0;
        } else if text.contains("halfmark") {
            v[1] = 0.6;
            v[2] = 0.8;
        } else {
            v[3] = 1.0;
        }
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
    index_body(store, path, &format!("alphamark note {tag}\n"), age_days);
}

/// Index `path` with `body`, last modified `age_days` ago.
fn index_body(store: &mut Store, path: &str, body: &str, age_days: Option<u64>) {
    store.index_document("notes", path, path, body).unwrap();
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

// ── `search` (BM25) and `vsearch` (vector) ────────────────────────────────────

/// Which of the two single-signal searches a test drives.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Keyword,
    Vector,
}

const KINDS: [Kind; 2] = [Kind::Keyword, Kind::Vector];

fn find(
    store: &mut Store,
    kind: Kind,
    limit: usize,
    per_file: usize,
    recency: Option<Recency>,
) -> Vec<SearchResult> {
    let options = SearchOptions { per_file, recency };
    match kind {
        Kind::Keyword => store.search_fts_multi_with("alphamark", limit, None, &options),
        Kind::Vector => store.search_vec_multi_with("alphamark", limit, None, &options),
    }
    .unwrap()
}

/// About `chunks` chunks of plain lines with "alphamark" placed in the middle
/// of each chunk listed in `marked`. `tag` keeps the body the same length for
/// every document.
fn sectioned(tag: &str, chunks: usize, marked: &[usize]) -> String {
    let mut text: String = (0..)
        .map(|i| format!("Sentence {i} of {tag} says nothing in particular.\n"))
        .scan(0, |len, line| {
            *len += line.len();
            (*len < 3100 * chunks + 2000).then_some(line)
        })
        .collect();
    let plan = chunk_document(&text);
    assert!(plan.len() >= chunks, "filler must span {chunks} chunks");
    let mut marked = marked.to_vec();
    marked.sort_by_key(|chunk| std::cmp::Reverse(*chunk));
    for chunk in marked {
        let from = if chunk == 0 {
            0
        } else {
            plan[chunk - 1].pos + plan[chunk - 1].text.len()
        };
        let to = plan.get(chunk + 1).map_or(text.len(), |c| c.pos);
        let at = text[(from + to) / 2..]
            .find('\n')
            .map_or(text.len(), |n| (from + to) / 2 + n + 1);
        text.insert_str(at, "alphamark\n");
    }
    text
}

/// Two files equally relevant to a keyword or vector search: the one modified
/// later ranks first, whichever of the two it is.
#[test]
fn ac_9_search_ranks_the_newer_of_two_equally_relevant_files_first() {
    for kind in KINDS {
        for (a_days, b_days, first) in [(400, 2, "b.md"), (2, 400, "a.md")] {
            let dir = TempDir::new().unwrap();
            let mut store = open_store(&dir);
            index(&mut store, "a.md", "one", Some(a_days));
            index(&mut store, "b.md", "two", Some(b_days));
            store.flush().unwrap();

            let weighted = find(&mut store, kind, 10, 1, Some(thirty_days()));

            assert_eq!(weighted.len(), 2, "{kind:?}");
            assert_eq!(
                weighted[0].path, first,
                "{kind:?}: a {a_days}d, b {b_days}d"
            );
        }
    }
}

/// A strongly relevant old file is down-weighted by at most the floor: it
/// still appears, and still beats a recent file that barely matches.
#[test]
fn ac_10_search_keeps_a_strongly_relevant_old_file() {
    for kind in KINDS {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir);
        index_body(
            &mut store,
            "old.md",
            "alphamark alphamark alphamark alphamark alphamark alphamark\n",
            Some(3000),
        );
        // The recent file barely matches: one mention among filler for the
        // keyword search, no match at all for the vector search.
        let filler = "plain words about nothing in particular. ".repeat(40);
        let recent = match kind {
            Kind::Keyword => format!("alphamark {filler}\n"),
            Kind::Vector => format!("unrelated {filler}\n"),
        };
        index_body(&mut store, "new.md", &recent, Some(1));
        store.flush().unwrap();

        let plain = find(&mut store, kind, 10, 1, None);
        let weighted = find(&mut store, kind, 10, 1, Some(thirty_days()));

        assert_eq!(paths(&plain)[0], "old.md", "{kind:?}");
        assert_eq!(
            paths(&weighted)[0],
            "old.md",
            "{kind:?}: {:?}",
            paths(&weighted)
        );
        assert_eq!(weighted.len(), plain.len(), "{kind:?}");
        let (p, w) = (plain[0].score, weighted[0].score);
        assert!(
            w >= p * DEFAULT_RECENCY_FLOOR - 1e-5 && w < p,
            "{kind:?}: {w} vs {p}"
        );
    }
}

/// A recent file just outside the plain top `limit` is found: the candidate
/// pool is wider than the cut, so weighting can lift it into the results.
#[test]
fn ac_11_search_surfaces_a_recent_file_outside_the_plain_cut() {
    for kind in KINDS {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir);
        // Four old files outrank the recent one: three mentions each for the
        // keyword search, three matching sections each for the vector search
        // (twelve vectors that all outrank the recent file's one weaker one).
        for tag in ["one", "two", "three", "four"] {
            let body = match kind {
                Kind::Keyword => format!("alphamark alphamark alphamark note {tag}\n"),
                Kind::Vector => sectioned(tag, 4, &[0, 1, 3]),
            };
            index_body(&mut store, &format!("old-{tag}.md"), &body, Some(400));
        }
        let recent = match kind {
            Kind::Keyword => "alphamark note\n",
            Kind::Vector => "halfmark note\n",
        };
        index_body(&mut store, "z-recent.md", recent, Some(1));
        store.flush().unwrap();

        let plain = find(&mut store, kind, 2, 1, None);
        let weighted = find(&mut store, kind, 2, 1, Some(thirty_days()));

        assert_eq!(plain.len(), 2, "{kind:?}");
        assert!(!paths(&plain).contains(&"z-recent.md"), "{kind:?}");
        assert_eq!(weighted.len(), 2, "{kind:?}");
        assert_eq!(
            weighted[0].path,
            "z-recent.md",
            "{kind:?}: {:?}",
            paths(&weighted)
        );
    }
}

/// With no recency the results are exactly what they were: the options used
/// before the field existed, the default options and an explicit `None` agree
/// on order and score, whatever mtimes the index holds.
#[test]
fn ac_12_search_without_recency_is_unchanged() {
    for kind in KINDS {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir);
        index(&mut store, "a.md", "one", Some(900));
        index(&mut store, "b.md", "two", Some(1));
        index_body(&mut store, "c.md", "alphamark alphamark note\n", None);
        store.flush().unwrap();

        let before = match kind {
            Kind::Keyword => store.search_fts_multi("alphamark", 10, None),
            Kind::Vector => store.search_vec_multi("alphamark", 10, None),
        }
        .unwrap();
        let with = find(&mut store, kind, 10, 1, None);
        for path in ["a.md", "b.md", "c.md"] {
            db::set_document_stat(&store.db, "notes", path, None).unwrap();
        }
        let unstamped = find(&mut store, kind, 10, 1, None);

        assert_eq!(before.len(), 3, "{kind:?}");
        for other in [&with, &unstamped] {
            assert_eq!(paths(other), paths(&before), "{kind:?}");
            for (x, y) in before.iter().zip(other.iter()) {
                assert_eq!(x.score.to_bits(), y.score.to_bits(), "{kind:?} {}", x.path);
            }
        }
    }
}

/// Each file's score is its unweighted score times the factor of its age. A
/// file with no recorded mtime counts as new and a future mtime is not
/// penalised.
#[test]
fn ac_13_search_scores_scale_by_age_and_missing_or_future_mtime_is_new() {
    for kind in KINDS {
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

        let plain = find(&mut store, kind, 10, 1, None);
        let weighted = find(&mut store, kind, 10, 1, Some(thirty_days()));
        let ratio = |path: &str| {
            let score = |rs: &[SearchResult]| rs.iter().find(|r| r.path == path).unwrap().score;
            score(&weighted) / score(&plain)
        };
        let r = thirty_days();

        assert!(
            close(ratio("a.md"), r.factor(Duration::from_secs(10 * DAY))),
            "{kind:?}"
        );
        assert!(
            close(ratio("c.md"), r.factor(Duration::from_secs(100 * DAY))),
            "{kind:?}"
        );
        assert!(close(ratio("m.md"), 1.0), "{kind:?}");
        assert!(close(ratio("x.md"), 1.0), "{kind:?}");
    }
}

/// Equal weighted scores order by path, run after run.
#[test]
fn ac_14_search_orders_equal_weighted_scores_by_path() {
    for kind in KINDS {
        let dir = TempDir::new().unwrap();
        let mut store = open_store(&dir);
        for (path, tag) in [("c.md", "one"), ("a.md", "two"), ("b.md", "six")] {
            index(&mut store, path, tag, Some(30));
        }
        // The same instant for every file: the factors are equal.
        let at = (SystemTime::now() - Duration::from_secs(30 * DAY))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        for path in ["a.md", "b.md", "c.md"] {
            db::set_document_stat(&store.db, "notes", path, Some((1, at))).unwrap();
        }
        store.flush().unwrap();

        for _ in 0..3 {
            let weighted = find(&mut store, kind, 10, 1, Some(thirty_days()));
            assert_eq!(paths(&weighted), ["a.md", "b.md", "c.md"], "{kind:?}");
        }
    }
}

/// Sections per file work with weighting: a file keeps all its sections and
/// they follow the order of the file's weighted score.
#[test]
fn ac_15_search_per_file_sections_work_with_recency() {
    for kind in KINDS {
        for (a_days, b_days, first) in [(400, 2, "b.md"), (2, 400, "a.md")] {
            let dir = TempDir::new().unwrap();
            let mut store = open_store(&dir);
            index_body(
                &mut store,
                "a.md",
                &sectioned("one", 4, &[1, 3]),
                Some(a_days),
            );
            index_body(
                &mut store,
                "b.md",
                &sectioned("two", 4, &[1, 3]),
                Some(b_days),
            );
            store.flush().unwrap();

            let weighted = find(&mut store, kind, 10, 2, Some(thirty_days()));

            assert_eq!(weighted.len(), 4, "{kind:?}");
            assert_eq!(weighted[0].path, first, "{kind:?}");
            assert_eq!(weighted[1].path, first, "{kind:?}");
            assert_ne!(
                weighted[0].best_chunk_pos, weighted[1].best_chunk_pos,
                "{kind:?}"
            );
            assert_ne!(weighted[2].path, first, "{kind:?}");
            assert_eq!(weighted[2].path, weighted[3].path, "{kind:?}");
        }
    }
}

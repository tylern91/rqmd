//! Sections per file: `SearchOptions::per_file` on keyword, vector and hybrid
//! search. No model is needed: a marker backend embeds and reranks by the
//! marker words a text contains.

use std::sync::{Arc, Mutex};

use rqmd_core::chunking::chunk_document;
use rqmd_core::{
    Deadline, MAX_PER_FILE, SearchOptions, SearchResult, Store, StoreConfig, clamp_per_file,
};
use tempfile::TempDir;

/// Embeds "alphamark" text onto axis 1, "halfmark" text between axes 1 and 2
/// (cosine 0.6 with an "alphamark" query), and everything else onto axis 3.
/// Reranks a text by how many times it contains "alphamark", and records the
/// size of every rerank call.
struct MarkerBackend {
    rerank_sizes: Arc<Mutex<Vec<usize>>>,
}

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
        self.rerank_sizes.lock().unwrap().push(docs.len());
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

fn open_store(dir: &TempDir) -> (Store, Arc<Mutex<Vec<usize>>>) {
    let sizes = Arc::new(Mutex::new(Vec::new()));
    let config = StoreConfig {
        db_path: dir.path().join("test.sqlite"),
        tantivy_dir: dir.path().join("tantivy"),
        hnsw_path: dir.path().join("hnsw.usearch"),
        read_only: false,
    };
    let store = Store::open(
        config,
        Box::new(MarkerBackend {
            rerank_sizes: Arc::clone(&sizes),
        }),
    )
    .unwrap();
    (store, sizes)
}

/// About `chunks` chunks of plain lines with each `(chunk, sentence)` of
/// `marks` placed in the middle of that chunk's own stretch of text, clear of
/// the overlap it shares with its neighbours. `tag` makes the body unique to
/// its document.
fn note(tag: &str, chunks: usize, marks: &[(usize, &str)]) -> String {
    let mut text: String = (0..)
        .map(|i| format!("Sentence {i} of {tag} says nothing in particular.\n"))
        .scan(0, |len, line| {
            *len += line.len();
            (*len < 3100 * chunks + 2000).then_some(line)
        })
        .collect();
    let plan = chunk_document(&text);
    assert!(plan.len() >= chunks, "filler must span {chunks} chunks");
    let mut marks = marks.to_vec();
    marks.sort_by_key(|(chunk, _)| std::cmp::Reverse(*chunk));
    for (chunk, sentence) in marks {
        let from = if chunk == 0 {
            0
        } else {
            plan[chunk - 1].pos + plan[chunk - 1].text.len()
        };
        let to = plan.get(chunk + 1).map_or(text.len(), |c| c.pos);
        let at = text[(from + to) / 2..]
            .find('\n')
            .map_or(text.len(), |n| (from + to) / 2 + n + 1);
        text.insert_str(at, &format!("{sentence}\n"));
    }
    text
}

fn index(store: &mut Store, collection: &str, path: &str, body: &str) {
    store.index_document(collection, path, path, body).unwrap();
}

fn opts(per_file: usize) -> SearchOptions {
    SearchOptions {
        per_file,
        ..SearchOptions::default()
    }
}

fn sections_of(results: &[SearchResult], file: &str) -> Vec<(usize, f32)> {
    results
        .iter()
        .filter(|r| r.file == format!("rqmd://{file}"))
        .map(|r| (r.best_chunk_pos, r.score))
        .collect()
}

/// A fixture is only valid when no marker falls inside the overlap of two
/// chunks, since that would match two chunks for one marker.
fn assert_one_chunk_per_marker(body: &str, marker: &str) {
    let hits = chunk_document(body)
        .iter()
        .filter(|c| c.text.contains(marker))
        .count();
    let marks = body.matches(marker).count();
    assert_eq!(hits, marks, "fixture markers must each sit in one chunk");
}

#[test]
fn ac_1_default_is_one_result_per_file_and_options_default_matches_it() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let body = note("a", 7, &[(1, "alphamark"), (3, "alphamark")]);
    assert_one_chunk_per_marker(&body, "alphamark");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    assert_eq!(SearchOptions::default().per_file, 1);
    let plain = store.search_fts_multi("alphamark", 10, None).unwrap();
    let with = store
        .search_fts_multi_with("alphamark", 10, None, &SearchOptions::default())
        .unwrap();
    assert_eq!(plain.len(), 1);
    assert_eq!(with.len(), 1);
    assert_eq!(plain[0].best_chunk_pos, with[0].best_chunk_pos);
    assert_eq!(
        plain[0].best_chunk_pos, 0,
        "keyword snippet stays the opening chunk"
    );

    let vec_plain = store.search_vec_multi("alphamark", 10, None).unwrap();
    let vec_with = store
        .search_vec_multi_with("alphamark", 10, None, &SearchOptions::default())
        .unwrap();
    assert_eq!(vec_plain.len(), 1);
    assert_eq!(vec_with.len(), 1);
    assert_eq!(vec_plain[0].score, vec_with[0].score);
}

#[test]
fn ac_2_vector_search_shows_the_matched_chunk() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let body = note("a", 7, &[(3, "alphamark")]);
    assert_one_chunk_per_marker(&body, "alphamark");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    let hits = store.search_vec_multi("alphamark", 10, None).unwrap();

    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].best_chunk.contains("alphamark"),
        "{:?}",
        hits[0].best_chunk
    );
    assert!(hits[0].best_chunk_pos > 0);
    assert!(body[hits[0].best_chunk_pos..].starts_with(hits[0].best_chunk.trim_start()));
}

#[test]
fn ac_3_vector_search_returns_the_best_sections_of_a_file_in_score_order() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let body = note(
        "a",
        7,
        &[(1, "alphamark"), (3, "alphamark"), (5, "halfmark")],
    );
    assert_one_chunk_per_marker(&body, "alphamark");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    let hits = store
        .search_vec_multi_with("alphamark", 10, None, &opts(3))
        .unwrap();

    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.file == "rqmd://notes/long.md"));
    assert!(hits[0].best_chunk.contains("alphamark"));
    assert!(hits[1].best_chunk.contains("alphamark"));
    assert!(hits[2].best_chunk.contains("halfmark"));
    assert!(hits[0].score >= hits[1].score && hits[1].score > hits[2].score);
    let mut positions: Vec<usize> = hits.iter().map(|h| h.best_chunk_pos).collect();
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(positions.len(), 3, "each section is a different chunk");
}

#[test]
fn ac_4_per_file_is_clamped_to_the_cap() {
    assert_eq!(MAX_PER_FILE, 5);
    assert_eq!(clamp_per_file(0), 1);
    assert_eq!(clamp_per_file(1), 1);
    assert_eq!(clamp_per_file(5), 5);
    assert_eq!(clamp_per_file(6), 5);
    assert_eq!(clamp_per_file(usize::MAX), 5);

    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let marks: Vec<(usize, &str)> = (0..9).map(|n| (n, "alphamark")).collect();
    let body = note("a", 9, &marks);
    assert_one_chunk_per_marker(&body, "alphamark");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    for (asked, expected) in [(99, 5), (usize::MAX, 5), (0, 1), (5, 5), (2, 2)] {
        let vec = store
            .search_vec_multi_with("alphamark", 50, None, &opts(asked))
            .unwrap();
        assert_eq!(vec.len(), expected, "vsearch per_file={asked}");
        let fts = store
            .search_fts_multi_with("alphamark", 50, None, &opts(asked))
            .unwrap();
        assert_eq!(fts.len(), expected, "search per_file={asked}");
        let hybrid = store
            .hybrid_query_outcome_with(
                "alphamark",
                None,
                50,
                None,
                false,
                true,
                Deadline::none(),
                true,
                &opts(asked),
            )
            .unwrap()
            .results;
        assert_eq!(hybrid.len(), expected, "query per_file={asked}");
    }
}

#[test]
fn ac_5_sections_are_ordered_by_score_then_path() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let marks = [(1, "alphamark"), (3, "alphamark"), (5, "halfmark")];
    // Added out of path order, with bodies that differ only by tag.
    index(&mut store, "notes", "b.md", &note("b", 7, &marks));
    index(&mut store, "notes", "a.md", &note("a", 7, &marks));
    index(
        &mut store,
        "notes",
        "c.md",
        &note("c", 7, &[(2, "halfmark")]),
    );
    store.flush().unwrap();

    let hits = store
        .search_vec_multi_with("alphamark", 50, None, &opts(2))
        .unwrap();

    let order: Vec<(&str, usize)> = hits
        .iter()
        .map(|h| (h.path.as_str(), h.best_chunk_pos))
        .collect();
    assert_eq!(
        hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
        ["a.md", "a.md", "b.md", "b.md", "c.md", "c.md"],
        "{order:?}"
    );
    for pair in hits.windows(2) {
        let (x, y) = (&pair[0], &pair[1]);
        assert!(x.score >= y.score, "score must not rise: {order:?}");
        if x.score == y.score {
            assert!(x.file <= y.file, "equal scores sort by path: {order:?}");
            if x.file == y.file {
                assert!(
                    x.best_chunk_pos < y.best_chunk_pos,
                    "equal scores in one file keep document order: {order:?}"
                );
            }
        }
    }
}

#[test]
fn ac_6_the_limit_counts_sections() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let marks = [(1, "alphamark"), (3, "alphamark")];
    index(&mut store, "notes", "a.md", &note("a", 7, &marks));
    index(&mut store, "notes", "b.md", &note("b", 7, &marks));
    store.flush().unwrap();

    let hits = store
        .search_vec_multi_with("alphamark", 3, None, &opts(2))
        .unwrap();

    assert_eq!(hits.len(), 3);
    assert_eq!(sections_of(&hits, "notes/a.md").len(), 2);
    assert_eq!(sections_of(&hits, "notes/b.md").len(), 1);
}

#[test]
fn ac_7_keyword_search_returns_the_sections_that_contain_the_terms() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    let body = note(
        "a",
        7,
        &[(1, "needleword"), (3, "needleword"), (5, "needleword")],
    );
    assert_one_chunk_per_marker(&body, "needleword");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    let hits = store
        .search_fts_multi_with("needleword", 10, None, &opts(3))
        .unwrap();

    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.best_chunk.contains("needleword")));
    let mut positions: Vec<usize> = hits.iter().map(|h| h.best_chunk_pos).collect();
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(positions.len(), 3);

    let two = store
        .search_fts_multi_with("needleword", 10, None, &opts(2))
        .unwrap();
    assert_eq!(two.len(), 2);
    assert!(
        two.iter()
            .all(|t| hits.iter().any(|h| h.best_chunk_pos == t.best_chunk_pos)),
        "a smaller N returns a subset of the same sections"
    );
}

#[test]
fn ac_8_query_returns_reranked_sections_of_a_file_best_first() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    // Part 1 holds the marker three times, part 3 once, part 5 not at all.
    let body = note(
        "a",
        7,
        &[(1, "alphamark alphamark alphamark"), (3, "alphamark")],
    );
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    let mut query = |per_file| {
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
                &opts(per_file),
            )
            .unwrap()
            .results
    };
    let default = query(1);
    let sections = query(3);

    assert_eq!(default.len(), 1);
    assert_eq!(
        sections.len(),
        2,
        "only sections that contain the query term"
    );
    assert!(sections[0].score > sections[1].score);
    assert_eq!(sections[0].best_chunk.matches("alphamark").count(), 3);
    assert_eq!(sections[1].best_chunk.matches("alphamark").count(), 1);
    assert_ne!(sections[0].best_chunk_pos, sections[1].best_chunk_pos);
    assert!(sections.iter().all(|s| s.docid == sections[0].docid));
}

#[test]
fn ac_9_extra_sections_do_not_raise_the_rerank_pool_past_its_ceiling() {
    let dir = TempDir::new().unwrap();
    let (mut store, sizes) = open_store(&dir);
    let marks: Vec<(usize, &str)> = (0..6).map(|n| (n, "alphamark")).collect();
    for n in 0..60 {
        index(
            &mut store,
            "notes",
            &format!("d{n:02}.md"),
            &note(&format!("d{n}"), 6, &marks),
        );
    }
    store.flush().unwrap();

    let results = store
        .hybrid_query_outcome_with(
            "alphamark",
            None,
            30,
            None,
            false,
            true,
            Deadline::none(),
            true,
            &opts(5),
        )
        .unwrap()
        .results;

    assert_eq!(results.len(), 30);
    let total: usize = sizes.lock().unwrap().iter().sum();
    assert!(total <= 100, "reranked {total} chunks, ceiling is 100");
    assert!(total >= 60, "every candidate file keeps its best section");
}

#[test]
fn ac_10_a_match_inside_the_overlap_of_two_chunks_is_one_section() {
    let dir = TempDir::new().unwrap();
    let (mut store, _) = open_store(&dir);
    // The sentence sits in the stretch that chunks 0 and 1 share, and nowhere else.
    let plain = note("a", 4, &[]);
    let plan = chunk_document(&plain);
    let at = plain[plan[1].pos + 100..].find('\n').unwrap() + plan[1].pos + 100 + 1;
    let mut body = plain.clone();
    body.insert_str(at, "needleword\n");
    let holding = chunk_document(&body)
        .iter()
        .filter(|c| c.text.contains("needleword"))
        .count();
    assert_eq!(holding, 2, "fixture must put the sentence in two chunks");
    index(&mut store, "notes", "long.md", &body);
    store.flush().unwrap();

    let fts = store
        .search_fts_multi_with("needleword", 10, None, &opts(3))
        .unwrap();
    let hybrid = store
        .hybrid_query_outcome_with(
            "needleword",
            None,
            10,
            None,
            true,
            true,
            Deadline::none(),
            true,
            &opts(3),
        )
        .unwrap()
        .results;

    assert_eq!(fts.len(), 1, "search");
    assert_eq!(hybrid.len(), 1, "query");
}

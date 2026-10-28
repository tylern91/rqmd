//! Gate for `llama-cpp-2` (llama.cpp) bumps: do the stored embeddings survive the new engine?
//!
//! The embed fingerprint does not name the inference engine, and unchanged chunks of an edited
//! document reuse their stored vectors. A bump that shifts embedding values would therefore leave
//! old-engine and new-engine vectors mixed under one fingerprint. This gate measures the shift:
//! the cosine similarity of the old and the new vector for each text of a fixed sample must be at
//! least [`COSINE_THRESHOLD`]. Procedure and consequences: CONTRIBUTING.md, "llama.cpp bump gate".
//!
//! Two `#[ignore]` tests, run by hand with the embedding model in the hf-hub cache:
//!
//! 1. On the OLD engine (before the bump), `engine_gate_capture` writes the vectors to a JSON file.
//! 2. On the NEW engine, `engine_gate_compare` embeds the same sample, prints the cosine of every
//!    pair and fails when any pair is below the threshold.
//!
//! ```text
//! RQMD_ENGINE_GATE_REFERENCE=/path/reference.json RQMD_ENGINE_GATE_LABEL=llama-cpp-2-0.1.154 \
//!   cargo test -p rqmd-llm --test engine_bump_gate -- --ignored --nocapture engine_gate_capture
//! RQMD_ENGINE_GATE_REFERENCE=/path/reference.json RQMD_ENGINE_GATE_LABEL=llama-cpp-2-NEW \
//!   cargo test -p rqmd-llm --test engine_bump_gate -- --ignored --nocapture engine_gate_compare
//! ```
//!
//! The cosine, threshold and pairing logic is plain code with ordinary tests that need no model,
//! so CI covers it. Nothing here touches the index, the fingerprint or any runtime path.

use anyhow::{Context, Result, bail, ensure};
use rqmd_llm::{EMBED_DIM, InferenceBackend, LlamaCppBackend, LlamaCppConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Every old/new pair must reach this cosine similarity. Below it the only correct upgrade path
/// is `rqmd embed --rebuild`.
const COSINE_THRESHOLD: f64 = 0.999;

/// Names the file the capture step writes and the compare step reads (required, no default).
const ENV_REFERENCE: &str = "RQMD_ENGINE_GATE_REFERENCE";
/// Free-text label stored in the reference file and printed by compare (optional).
const ENV_LABEL: &str = "RQMD_ENGINE_GATE_LABEL";

const SAMPLE_JSON: &str = include_str!("fixtures/engine_bump_sample.json");

// ── Data ─────────────────────────────────────────────────────────────────────

/// A text of the fixed sample. `repeat` concatenates `text` that many times, so the committed
/// fixture stays readable while the long samples reach the truncation limit.
///
/// Token counts under the EmbeddingGemma tokenizer with the passage prefix, measured once with
/// `LlamaModel::str_to_token`: `long_near_limit` 2009, just under the 2044-token limit
/// (`safe_limit` in `LlamaCppBackend::embed_texts`); `long_over_limit` about 2210, so it is
/// truncated. A model or tokenizer change moves these numbers.
#[derive(Debug, Deserialize)]
struct Sample {
    id: String,
    text: String,
    #[serde(default = "one")]
    repeat: usize,
}

fn one() -> usize {
    1
}

#[derive(Debug, Deserialize)]
struct SampleFile {
    schema: u32,
    samples: Vec<Sample>,
}

/// One embedding, named `<sample id>/<kind>` where kind is `passage` or `query`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    id: String,
    vector: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reference {
    schema: u32,
    label: String,
    embed_model: String,
    /// `RQMD_FORCE_CPU` was set when the vectors were captured.
    force_cpu: bool,
    dim: usize,
    entries: Vec<Entry>,
}

fn load_samples() -> Result<Vec<Sample>> {
    let file: SampleFile = serde_json::from_str(SAMPLE_JSON).context("parse sample fixture")?;
    ensure!(
        file.schema == 1,
        "unsupported sample schema {}",
        file.schema
    );
    Ok(file.samples)
}

fn sample_text(sample: &Sample) -> String {
    sample.text.repeat(sample.repeat)
}

// ── Pure logic (covered by the non-ignored tests) ────────────────────────────

/// Cosine similarity of two vectors, accumulated in f64.
fn cosine(a: &[f32], b: &[f32]) -> Result<f64, String> {
    if a.len() != b.len() {
        return Err(format!("dimension mismatch: {} vs {}", a.len(), b.len()));
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if !(na.is_finite() && nb.is_finite() && dot.is_finite()) {
        return Err("non-finite value in a vector".to_string());
    }
    if na == 0.0 || nb == 0.0 {
        return Err("zero-length vector".to_string());
    }
    Ok(dot / (na.sqrt() * nb.sqrt()))
}

/// A pair passes when its cosine is a real number at or above `threshold`.
fn meets_threshold(cosine: f64, threshold: f64) -> bool {
    cosine.is_finite() && cosine >= threshold
}

#[derive(Debug, PartialEq)]
struct PairResult {
    id: String,
    cosine: f64,
    pass: bool,
}

#[derive(Debug, Default, PartialEq)]
struct Comparison {
    pairs: Vec<PairResult>,
    /// Unusable pairs: a missing or extra id, a duplicate id, or a cosine that could not be
    /// computed. Each one fails the gate.
    problems: Vec<String>,
}

impl Comparison {
    fn passed(&self) -> bool {
        self.problems.is_empty() && !self.pairs.is_empty() && self.pairs.iter().all(|p| p.pass)
    }

    fn lowest(&self) -> Option<&PairResult> {
        self.pairs
            .iter()
            .min_by(|a, b| a.cosine.total_cmp(&b.cosine))
    }
}

/// Pair `reference` with `current` by id and score every pair against `threshold`.
fn compare(reference: &[Entry], current: &[Entry], threshold: f64) -> Comparison {
    let mut out = Comparison::default();
    let mut seen = std::collections::HashSet::new();
    for old in reference {
        if !seen.insert(old.id.as_str()) {
            out.problems
                .push(format!("{}: duplicate id in the reference", old.id));
            continue;
        }
        let mut matches = current.iter().filter(|c| c.id == old.id);
        let Some(new) = matches.next() else {
            out.problems
                .push(format!("{}: missing from the current run", old.id));
            continue;
        };
        if matches.next().is_some() {
            out.problems
                .push(format!("{}: duplicate id in the current run", old.id));
            continue;
        }
        match cosine(&old.vector, &new.vector) {
            Ok(c) => out.pairs.push(PairResult {
                id: old.id.clone(),
                cosine: c,
                pass: meets_threshold(c, threshold),
            }),
            Err(why) => out.problems.push(format!("{}: {why}", old.id)),
        }
    }
    for new in current {
        if !reference.iter().any(|r| r.id == new.id) {
            out.problems
                .push(format!("{}: not in the reference", new.id));
        }
    }
    out
}

// ── Model-backed steps ───────────────────────────────────────────────────────

fn reference_path() -> Result<PathBuf> {
    std::env::var_os(ENV_REFERENCE)
        .map(PathBuf::from)
        .with_context(|| format!("set {ENV_REFERENCE} to the reference JSON file path"))
}

fn label() -> String {
    std::env::var(ENV_LABEL).unwrap_or_else(|_| "unlabelled".to_string())
}

fn force_cpu() -> bool {
    std::env::var("RQMD_FORCE_CPU")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Embed every sample as a passage and as a query with the backend this build links.
fn embed_sample(backend: &mut LlamaCppBackend) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for sample in load_samples()? {
        let text = sample_text(&sample);
        let passage = backend
            .embed_passage(&text)
            .with_context(|| format!("embed {} as passage", sample.id))?;
        entries.push(Entry {
            id: format!("{}/passage", sample.id),
            vector: passage,
        });
        let query = backend
            .embed_query(&text)
            .with_context(|| format!("embed {} as query", sample.id))?;
        entries.push(Entry {
            id: format!("{}/query", sample.id),
            vector: query,
        });
    }
    Ok(entries)
}

fn new_backend() -> Result<LlamaCppBackend> {
    // Resolves the three GGUF files through the hf-hub cache like the CLI does. Set
    // HF_HUB_OFFLINE=1 to forbid any download.
    LlamaCppBackend::new(LlamaCppConfig::default()).context("load the llama.cpp backend")
}

/// Run on the OLD engine. Writes the reference file; refuses to overwrite one, so a second run on
/// the new engine cannot replace the old vectors by accident.
#[test]
#[ignore = "needs the embedding GGUF in the hf-hub cache; run by hand on the old engine"]
fn engine_gate_capture() -> Result<()> {
    let path = reference_path()?;
    if path.exists() {
        bail!(
            "{} already exists; delete it first (it must hold the OLD engine's vectors)",
            path.display()
        );
    }
    let mut backend = new_backend()?;
    let entries = embed_sample(&mut backend)?;
    for e in &entries {
        ensure!(
            e.vector.len() == EMBED_DIM,
            "{}: {} dimensions, expected {EMBED_DIM}",
            e.id,
            e.vector.len()
        );
    }
    let reference = Reference {
        schema: 1,
        label: label(),
        embed_model: backend.embed_model_name().to_string(),
        force_cpu: force_cpu(),
        dim: EMBED_DIM,
        entries,
    };
    std::fs::write(&path, serde_json::to_vec(&reference)?)
        .with_context(|| format!("write {}", path.display()))?;
    println!(
        "captured {} vectors ({}, model {}, force_cpu={}) -> {}",
        reference.entries.len(),
        reference.label,
        reference.embed_model,
        reference.force_cpu,
        path.display()
    );
    Ok(())
}

/// Run on the NEW engine. Fails when any pair is below [`COSINE_THRESHOLD`].
#[test]
#[ignore = "needs the embedding GGUF in the hf-hub cache; run by hand on the new engine"]
fn engine_gate_compare() -> Result<()> {
    let path = reference_path()?;
    let raw = std::fs::read(&path).with_context(|| {
        format!(
            "read {}; run engine_gate_capture on the old engine first",
            path.display()
        )
    })?;
    let reference: Reference =
        serde_json::from_slice(&raw).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        reference.schema == 1,
        "unsupported reference schema {}",
        reference.schema
    );

    let mut backend = new_backend()?;
    ensure!(
        reference.embed_model == backend.embed_model_name(),
        "the reference was captured with model {}, this run uses {}",
        reference.embed_model,
        backend.embed_model_name()
    );
    let current = embed_sample(&mut backend)?;
    let result = compare(&reference.entries, &current, COSINE_THRESHOLD);

    println!(
        "old: {} (force_cpu={})   new: {} (force_cpu={})   threshold: {COSINE_THRESHOLD}",
        reference.label,
        reference.force_cpu,
        label(),
        force_cpu()
    );
    for p in &result.pairs {
        println!(
            "{:<28} cosine {:.6}  {}",
            p.id,
            p.cosine,
            if p.pass { "ok" } else { "FAIL" }
        );
    }
    for problem in &result.problems {
        println!("PROBLEM {problem}");
    }
    if let Some(low) = result.lowest() {
        println!("lowest: {} at {:.6}", low.id, low.cosine);
    }
    assert!(
        result.passed(),
        "engine bump gate FAILED: stored vectors are not interchangeable with the new engine's. \
         Upgrade path is `rqmd embed --rebuild`; say so in the changelog."
    );
    println!("engine bump gate passed");
    Ok(())
}

// ── Tests that need no model ─────────────────────────────────────────────────

#[cfg(test)]
mod logic {
    use super::*;

    fn entry(id: &str, vector: &[f32]) -> Entry {
        Entry {
            id: id.to_string(),
            vector: vector.to_vec(),
        }
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = [0.3, -0.4, 0.5];
        assert!((cosine(&v, &v).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_of_orthogonal_and_opposite_vectors() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).unwrap().abs() < 1e-12);
        assert!((cosine(&[1.0, 2.0], &[-1.0, -2.0]).unwrap() + 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_ignores_vector_length() {
        let c = cosine(&[1.0, 2.0, 3.0], &[10.0, 20.0, 30.0]).unwrap();
        assert!((c - 1.0).abs() < 1e-12);
    }

    #[test]
    fn cosine_rejects_unusable_input() {
        assert!(
            cosine(&[1.0], &[1.0, 2.0])
                .unwrap_err()
                .contains("dimension")
        );
        assert!(
            cosine(&[0.0, 0.0], &[1.0, 0.0])
                .unwrap_err()
                .contains("zero")
        );
        assert!(
            cosine(&[f32::NAN, 1.0], &[1.0, 0.0])
                .unwrap_err()
                .contains("non-finite")
        );
        assert!(cosine(&[f32::INFINITY, 1.0], &[1.0, 0.0]).is_err());
    }

    #[test]
    fn threshold_is_inclusive_and_rejects_nan() {
        assert!(meets_threshold(0.999, 0.999));
        assert!(meets_threshold(1.0, 0.999));
        assert!(!meets_threshold(0.998_999, 0.999));
        assert!(!meets_threshold(f64::NAN, 0.999));
    }

    #[test]
    fn gate_threshold_is_the_documented_one() {
        assert_eq!(COSINE_THRESHOLD, 0.999);
    }

    #[test]
    fn compare_passes_when_every_pair_is_close() {
        let old = [
            entry("a/passage", &[1.0, 0.0]),
            entry("b/query", &[0.0, 1.0]),
        ];
        let new = [
            entry("b/query", &[0.0, 1.0]),
            entry("a/passage", &[1.0, 0.0]),
        ];
        let r = compare(&old, &new, COSINE_THRESHOLD);
        assert!(r.passed(), "{r:?}");
        assert_eq!(r.pairs.len(), 2);
    }

    #[test]
    fn compare_fails_when_one_pair_is_below_threshold() {
        // cosine([1,0], [1,0.05]) is about 0.99875, under 0.999.
        let old = [
            entry("a/passage", &[1.0, 0.0]),
            entry("b/query", &[0.0, 1.0]),
        ];
        let new = [
            entry("a/passage", &[1.0, 0.05]),
            entry("b/query", &[0.0, 1.0]),
        ];
        let r = compare(&old, &new, COSINE_THRESHOLD);
        assert!(!r.passed());
        let failing: Vec<_> = r.pairs.iter().filter(|p| !p.pass).collect();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].id, "a/passage");
        assert_eq!(r.lowest().unwrap().id, "a/passage");
    }

    #[test]
    fn compare_passes_a_pair_exactly_at_the_threshold() {
        let (a, b) = ([1.0f32, 0.0], [0.9f32, 0.3]);
        let at = cosine(&a, &b).unwrap();
        let r = compare(&[entry("x", &a)], &[entry("x", &b)], at);
        assert!(r.passed(), "{r:?}");
        let r = compare(&[entry("x", &a)], &[entry("x", &b)], at + 1e-9);
        assert!(!r.passed());
    }

    #[test]
    fn compare_fails_on_missing_extra_and_duplicate_ids() {
        let old = [entry("a", &[1.0]), entry("b", &[1.0])];
        let missing = compare(&old, &[entry("a", &[1.0])], COSINE_THRESHOLD);
        assert!(!missing.passed());
        assert!(missing.problems.iter().any(|p| p.starts_with("b:")));

        let extra = compare(
            &old,
            &[entry("a", &[1.0]), entry("b", &[1.0]), entry("c", &[1.0])],
            COSINE_THRESHOLD,
        );
        assert!(!extra.passed());
        assert!(extra.problems.iter().any(|p| p.starts_with("c:")));

        let dup = compare(
            &[entry("a", &[1.0]), entry("a", &[1.0])],
            &[entry("a", &[1.0])],
            COSINE_THRESHOLD,
        );
        assert!(!dup.passed());
    }

    #[test]
    fn compare_fails_on_dimension_mismatch_and_on_nothing_to_compare() {
        let r = compare(
            &[entry("a", &[1.0, 0.0])],
            &[entry("a", &[1.0])],
            COSINE_THRESHOLD,
        );
        assert!(!r.passed());
        assert!(!compare(&[], &[], COSINE_THRESHOLD).passed());
    }

    #[test]
    fn fixture_covers_the_required_text_kinds() {
        let samples = load_samples().unwrap();
        let ids: Vec<&str> = samples.iter().map(|s| s.id.as_str()).collect();
        for needed in [
            "short_en",
            "long_near_limit",
            "long_over_limit",
            "cjk_zh",
            "cjk_ja",
            "code_rust",
            "code_python",
        ] {
            assert!(ids.contains(&needed), "fixture lacks {needed}");
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "duplicate sample ids");
        assert!(
            samples
                .iter()
                .all(|s| !s.text.trim().is_empty() && s.repeat >= 1)
        );
        let len = |id: &str| sample_text(samples.iter().find(|s| s.id == id).unwrap()).len();
        assert!(
            len("long_near_limit") > 5_000,
            "near-limit text is too short to matter"
        );
        assert!(len("long_over_limit") > len("long_near_limit"));
        assert!(
            samples.iter().any(|s| !s.text.is_ascii()),
            "no non-ASCII sample"
        );
    }
}

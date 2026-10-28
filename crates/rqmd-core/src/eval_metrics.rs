//! Ranking-quality metrics for `rqmd eval --judgements`.
//!
//! Pure functions over a ranked list of document keys and the judged
//! documents for one query. They touch no index, clock or model, so every
//! value can be checked by hand (see the tests).
//!
//! Conventions shared by every function:
//!
//! - `ranked` is best first and holds each document at most once; the caller
//!   removes repeats (a document that matched several sections) before
//!   scoring. Rank 1 is `ranked[0]`.
//! - A document is *relevant* when its grade is at least 1. A grade of 0
//!   means "judged, not relevant" and counts for nothing.
//! - The cutoff `k` counts documents. `k == 0` scores 0.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Discount applied to the gain at 1-based `rank`: `1 / log2(rank + 1)`.
fn discount(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).log2()
}

/// Discounted cumulative gain of `ranked` over its first `k` entries:
/// the sum of `grade / log2(rank + 1)`. The gain is the grade itself (linear),
/// not `2^grade - 1`. A document with no entry in `grades` has grade 0.
pub fn dcg_at_k<T: Eq + Hash>(ranked: &[T], grades: &HashMap<T, u32>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, doc)| {
            let grade = grades.get(doc).copied().unwrap_or(0);
            f64::from(grade) * discount(i + 1)
        })
        .fold(0.0, |acc, gain| acc + gain)
}

/// nDCG@k: `DCG@k` of `ranked` divided by the `DCG@k` of the ideal order
/// (judged documents sorted by grade, highest first). It is 1.0 for a ranking
/// that already is the ideal order and 0.0 when no judged document has a grade
/// above 0 (the ideal gain is 0, so there is nothing to find).
pub fn ndcg_at_k<T: Eq + Hash>(ranked: &[T], grades: &HashMap<T, u32>, k: usize) -> f64 {
    let mut ideal: Vec<u32> = grades.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let ideal_dcg: f64 = ideal
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, grade)| f64::from(*grade) * discount(i + 1))
        .fold(0.0, |acc, gain| acc + gain);
    if ideal_dcg <= 0.0 {
        return 0.0;
    }
    dcg_at_k(ranked, grades, k) / ideal_dcg
}

/// Reciprocal rank at k: `1 / rank` of the first relevant document within the
/// first `k`, else 0.0. The mean over queries is MRR@k.
pub fn reciprocal_rank_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .position(|doc| relevant.contains(doc))
        .map_or(0.0, |i| 1.0 / (i + 1) as f64)
}

/// Recall@k: the share of the relevant documents found in the first `k`.
/// 0.0 when `relevant` is empty.
pub fn recall_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let found = ranked
        .iter()
        .take(k)
        .filter(|doc| relevant.contains(*doc))
        .count();
    found as f64 / relevant.len() as f64
}

/// Hit@k: whether any relevant document is in the first `k`. The mean of
/// this over queries (true as 1, false as 0) is the hit rate.
pub fn hit_at_k<T: Eq + Hash>(ranked: &[T], relevant: &HashSet<T>, k: usize) -> bool {
    ranked.iter().take(k).any(|doc| relevant.contains(doc))
}

/// Arithmetic mean; 0.0 for an empty slice.
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().fold(0.0, |acc, v| acc + v) / values.len() as f64
}

/// Nearest-rank percentile of `values` (unsorted input is fine): the value at
/// 1-based rank `ceil(p / 100 * n)`, never below rank 1. `None` for an empty
/// slice, a `p` outside 0..=100, or any NaN in `values`.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() || !(0.0..=100.0).contains(&p) || values.iter().any(|v| v.is_nan()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grades(pairs: &[(&'static str, u32)]) -> HashMap<&'static str, u32> {
        pairs.iter().copied().collect()
    }

    fn set(docs: &[&'static str]) -> HashSet<&'static str> {
        docs.iter().copied().collect()
    }

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    // Hand computation, graded judgements a=3, b=2, d=1 (c is judged 0):
    //   ranking [b, x, a, d]
    //   DCG@10  = 2/log2(2) + 0/log2(3) + 3/log2(4) + 1/log2(5)
    //           = 2 + 0 + 1.5 + 0.4306766 = 3.9306766
    //   ideal order [a, b, d] (grades 3, 2, 1, 0)
    //   IDCG@10 = 3/log2(2) + 2/log2(3) + 1/log2(4)
    //           = 3 + 1.2618595 + 0.5 = 4.7618595
    //   nDCG@10 = 3.9306766 / 4.7618595 = 0.8254499
    #[test]
    fn ndcg_graded_ranking() {
        let g = grades(&[("a", 3), ("b", 2), ("c", 0), ("d", 1)]);
        close(ndcg_at_k(&["b", "x", "a", "d"], &g, 10), 0.8254499);
        close(dcg_at_k(&["b", "x", "a", "d"], &g, 10), 3.9306766);
    }

    // Same ranking cut at k = 2:
    //   DCG@2  = 2/log2(2) + 0/log2(3) = 2
    //   IDCG@2 = 3/log2(2) + 2/log2(3) = 3 + 1.2618595 = 4.2618595
    //   nDCG@2 = 2 / 4.2618595 = 0.4692787
    #[test]
    fn ndcg_respects_the_cutoff() {
        let g = grades(&[("a", 3), ("b", 2), ("c", 0), ("d", 1)]);
        close(ndcg_at_k(&["b", "x", "a", "d"], &g, 2), 0.4692787);
    }

    // The ideal order scores 1: DCG equals IDCG. An empty ranking scores 0.
    #[test]
    fn ndcg_ideal_order_is_one_and_empty_is_zero() {
        let g = grades(&[("a", 3), ("b", 2), ("d", 1)]);
        close(ndcg_at_k(&["a", "b", "d"], &g, 10), 1.0);
        close(ndcg_at_k::<&str>(&[], &g, 10), 0.0);
        // Positive zero, so it prints as 0 and not -0.
        assert!(ndcg_at_k::<&str>(&[], &g, 10).is_sign_positive());
        close(ndcg_at_k(&["a"], &g, 0), 0.0);
    }

    // Binary judgements {a, b}: ranking [x, a, b]:
    //   DCG = 0 + 1/log2(3) + 1/log2(4) = 0.6309298 + 0.5 = 1.1309298
    //   IDCG = 1/log2(2) + 1/log2(3) = 1 + 0.6309298 = 1.6309298
    //   nDCG = 1.1309298 / 1.6309298 = 0.6934264
    #[test]
    fn ndcg_binary_judgements() {
        let g = grades(&[("a", 1), ("b", 1)]);
        close(ndcg_at_k(&["x", "a", "b"], &g, 10), 0.6934264);
    }

    // All judged documents have grade 0: the ideal gain is 0, the score is 0.
    #[test]
    fn ndcg_with_no_gain_is_zero() {
        let g = grades(&[("a", 0)]);
        close(ndcg_at_k(&["a"], &g, 10), 0.0);
    }

    // First relevant document at rank 3: RR = 1/3. With k = 2 it falls
    // outside the cutoff: RR = 0. With two relevant documents at ranks 2 and
    // 3 only the first counts: RR = 1/2.
    #[test]
    fn reciprocal_rank_uses_the_first_relevant_rank() {
        close(
            reciprocal_rank_at_k(&["x", "y", "r"], &set(&["r"]), 10),
            1.0 / 3.0,
        );
        close(reciprocal_rank_at_k(&["x", "y", "r"], &set(&["r"]), 2), 0.0);
        close(
            reciprocal_rank_at_k(&["x", "y", "r"], &set(&["y", "r"]), 10),
            0.5,
        );
        close(reciprocal_rank_at_k(&["r"], &set(&["r"]), 10), 1.0);
        close(reciprocal_rank_at_k(&["x"], &set(&["r"]), 10), 0.0);
    }

    // Relevant {a, b, c, d}, ranking [a, x, c, y, b]:
    //   within the first 3 : a, c       -> 2 / 4 = 0.5
    //   within the first 5 : a, c, b    -> 3 / 4 = 0.75
    //   within the first 1 : a          -> 1 / 4 = 0.25
    #[test]
    fn recall_counts_relevant_found_within_k() {
        let rel = set(&["a", "b", "c", "d"]);
        let ranked = ["a", "x", "c", "y", "b"];
        close(recall_at_k(&ranked, &rel, 3), 0.5);
        close(recall_at_k(&ranked, &rel, 5), 0.75);
        close(recall_at_k(&ranked, &rel, 1), 0.25);
        close(recall_at_k(&ranked, &set(&[]), 5), 0.0);
    }

    // Relevant {c}, ranking [a, b, c]: a hit from k = 3, not at k = 2.
    #[test]
    fn hit_is_true_once_a_relevant_document_is_in_range() {
        let rel = set(&["c"]);
        assert!(hit_at_k(&["a", "b", "c"], &rel, 3));
        assert!(!hit_at_k(&["a", "b", "c"], &rel, 2));
        assert!(!hit_at_k(&["a", "b", "c"], &rel, 0));
        assert!(!hit_at_k::<&str>(&[], &rel, 10));
    }

    // (1 + 0.5 + 0) / 3 = 0.5
    #[test]
    fn mean_of_values() {
        close(mean(&[1.0, 0.5, 0.0]), 0.5);
        close(mean(&[]), 0.0);
    }

    // Values 15, 20, 35, 40, 50 (n = 5), nearest rank = ceil(p/100 * 5):
    //   p50 -> ceil(2.5)  = 3 -> 35
    //   p95 -> ceil(4.75) = 5 -> 50
    //   p20 -> ceil(1.0)  = 1 -> 15
    //   p0  -> rank 0 raised to 1 -> 15,  p100 -> rank 5 -> 50
    // Input order does not matter.
    #[test]
    fn percentile_is_nearest_rank() {
        let v = [50.0, 15.0, 40.0, 20.0, 35.0];
        assert_eq!(percentile(&v, 50.0), Some(35.0));
        assert_eq!(percentile(&v, 95.0), Some(50.0));
        assert_eq!(percentile(&v, 20.0), Some(15.0));
        assert_eq!(percentile(&v, 0.0), Some(15.0));
        assert_eq!(percentile(&v, 100.0), Some(50.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&v, 101.0), None);
        assert_eq!(percentile(&[1.0, f64::NAN], 50.0), None);
    }
}

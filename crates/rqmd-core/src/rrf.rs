//! Reciprocal Rank Fusion — exact port of qmd's store.ts implementation.
//!
//! Formula: weight / (k + rank + 1)  where k=60
//! Original-query lists (FTS or vec on the raw query) get weight=2.0.
//! Expansion lists (lex/vec/hyde) get weight=1.0.
//! Top-rank bonuses: +0.05 if rank=0, +0.02 if rank<=2.

use std::collections::HashMap;

use crate::types::{QueryType, RankedListMeta, RankedResult};

const RRF_K: f32 = 60.0;

/// Return per-list weights based on query type.
pub fn rrf_weights(meta: &[RankedListMeta]) -> Vec<f32> {
    meta.iter()
        .map(|m| {
            if m.query_type == QueryType::Original {
                2.0
            } else {
                1.0
            }
        })
        .collect()
}

/// Fuse N ranked lists into a single ranked list using RRF.
///
/// `result_lists[i]` is sorted descending by backend score.
/// `weights[i]` is the RRF weight for that list (2.0 for original, 1.0 for expansions).
/// Returns results sorted by RRF score descending.
pub fn reciprocal_rank_fusion(
    result_lists: &[Vec<RankedResult>],
    weights: &[f32],
) -> Vec<RankedResult> {
    struct Entry {
        result: RankedResult,
        rrf_score: f32,
        top_rank: usize,
    }

    let mut scores: HashMap<String, Entry> = HashMap::new();

    for (list_idx, list) in result_lists.iter().enumerate() {
        let weight = weights.get(list_idx).copied().unwrap_or(1.0);
        for (rank, result) in list.iter().enumerate() {
            let contribution = weight / (RRF_K + rank as f32 + 1.0);
            match scores.get_mut(&result.filepath) {
                Some(entry) => {
                    entry.rrf_score += contribution;
                    if rank < entry.top_rank {
                        entry.top_rank = rank;
                    }
                }
                None => {
                    scores.insert(
                        result.filepath.clone(),
                        Entry {
                            result: result.clone(),
                            rrf_score: contribution,
                            top_rank: rank,
                        },
                    );
                }
            }
        }
    }

    // Top-rank bonus (mirrors store.ts exactly)
    for entry in scores.values_mut() {
        if entry.top_rank == 0 {
            entry.rrf_score += 0.05;
        } else if entry.top_rank <= 2 {
            entry.rrf_score += 0.02;
        }
    }

    let mut results: Vec<(f32, RankedResult)> = scores
        .into_values()
        .map(|e| {
            (
                e.rrf_score,
                RankedResult {
                    backend_score: e.rrf_score,
                    ..e.result
                },
            )
        })
        .collect();

    results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(filepath: &str, score: f32) -> RankedResult {
        RankedResult {
            filepath: filepath.to_string(),
            title: filepath.to_string(),
            backend_score: score,
        }
    }

    #[test]
    fn rrf_single_list() {
        let list = vec![result("a", 1.0), result("b", 0.8), result("c", 0.5)];
        let fused = reciprocal_rank_fusion(&[list], &[1.0]);
        assert_eq!(fused[0].filepath, "a");
        assert_eq!(fused[1].filepath, "b");
    }

    #[test]
    fn rrf_two_lists_agree() {
        let l1 = vec![result("a", 1.0), result("b", 0.8)];
        let l2 = vec![result("a", 0.9), result("c", 0.7)];
        let fused = reciprocal_rank_fusion(&[l1, l2], &[1.0, 1.0]);
        // "a" appears at rank 0 in both lists → highest score
        assert_eq!(fused[0].filepath, "a");
    }

    #[test]
    fn rrf_original_weight_dominates() {
        // "a" only in expansion list (weight=1), "b" only in original (weight=2)
        let orig = vec![result("b", 1.0)];
        let exp = vec![result("a", 1.0)];
        let fused = reciprocal_rank_fusion(&[orig, exp], &[2.0, 1.0]);
        assert_eq!(fused[0].filepath, "b");
    }
}

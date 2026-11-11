//! Ranking: the full-text query built from free text, vector similarity, and the fusion of both.

use std::collections::HashMap;
use std::hash::Hash;

/// How many candidates each ranking contributes before fusion.
pub(crate) const CANDIDATES: usize = 50;
const RRF_K: f64 = 60.0;
/// Distinct terms taken from a query; a pasted page of text must not become a page-long query.
const MAX_TERMS: usize = 48;

/// The FTS5 query for free text: its alphanumeric terms, each quoted, joined with `OR`. `None` when
/// the text has no such term.
pub(crate) fn fts_query(text: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for term in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let term = term.to_lowercase();
        if !terms.contains(&term) {
            terms.push(term);
            if terms.len() == MAX_TERMS {
                break;
            }
        }
    }
    if terms.is_empty() {
        return None;
    }
    let quoted: Vec<String> = terms.iter().map(|t| format!("\"{t}\"")).collect();
    Some(quoted.join(" OR "))
}

/// Reciprocal rank fusion of rankings given best first. Keys with equal scores keep the order in
/// which they first appear, earlier rankings first.
pub(crate) fn fuse<K: Clone + Eq + Hash>(rankings: &[&[K]]) -> Vec<K> {
    let mut scores: HashMap<&K, (f64, usize)> = HashMap::new();
    for ranking in rankings {
        for (rank, key) in ranking.iter().enumerate() {
            let seen = scores.len();
            scores.entry(key).or_insert((0.0, seen)).0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(&K, (f64, usize))> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.0.total_cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
    fused.into_iter().map(|(key, _)| key.clone()).collect()
}

/// Scales `vector` to unit length; a zero vector is left alone.
pub(crate) fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vector.iter_mut() {
            *x /= norm;
        }
    }
}

/// The stored form of a vector: its components as little-endian `f32`.
pub(crate) fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

pub(crate) fn from_blob(blob: &[u8]) -> Vec<f32> {
    let (components, _) = blob.as_chunks::<4>();
    components
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect()
}

/// Dot product, which is the cosine similarity for unit vectors. `None` when the dimensions differ
/// (a row embedded by a model of another size).
pub(crate) fn dot(a: &[f32], b: &[f32]) -> Option<f32> {
    (a.len() == b.len()).then(|| a.iter().zip(b).map(|(x, y)| x * y).sum())
}

/// Keeps the [`CANDIDATES`] highest-scoring entries, best first.
pub(crate) fn top<T>(mut scored: Vec<(T, f32)>) -> Vec<(T, f32)> {
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(CANDIDATES);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_alphanumeric_terms_once_each() {
        assert_eq!(
            fts_query("parse_config(\"x\") AND Parse!").as_deref(),
            Some("\"parse\" OR \"config\" OR \"x\" OR \"and\"")
        );
        assert_eq!(fts_query("naïve café").as_deref(), Some("\"naïve\" OR \"café\""));
        assert_eq!(fts_query(" \"*\" - ()"), None);
        let long: String = (0..200).map(|i| format!("w{i} ")).collect();
        assert_eq!(
            fts_query(&long).map(|q| q.matches(" OR ").count()),
            Some(MAX_TERMS - 1)
        );
    }

    #[test]
    fn fusion_rewards_agreement_and_keeps_first_seen_order_on_ties() {
        // `b` is second in both rankings and beats the leaders, which each appear once.
        assert_eq!(fuse(&[&["a", "b"], &["c", "b"]]), ["b", "a", "c"]);
        assert_eq!(fuse(&[&["a", "b", "c"], &[]]), ["a", "b", "c"]);
        assert_eq!(fuse::<&str>(&[&[], &[]]), Vec::<&str>::new());
    }

    #[test]
    fn vectors_survive_storage_and_are_compared_by_cosine() {
        let mut v = vec![3.0f32, 4.0];
        normalize(&mut v);
        assert_eq!(v, [0.6, 0.8]);
        assert_eq!(from_blob(&to_blob(&v)), v);
        assert_eq!(to_blob(&[1.0f32]), [0, 0, 0x80, 0x3f]);
        assert!((dot(&v, &v).unwrap() - 1.0).abs() < 1e-6);
        assert_eq!(dot(&v, &[1.0]), None);
        let mut zero = vec![0.0f32; 2];
        normalize(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
    }
}

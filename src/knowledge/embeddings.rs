use crate::{
    docs::{PER_FILE, Source, limit_per_file},
    engine,
};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{fmt, path::Path};

/// Why embedding failed, so callers can fall back to lexical (BM25) results.
/// Wrapped in `anyhow::Error` by `rerank`; use `err.downcast_ref::<EmbedError>()`.
#[derive(Clone, Debug, PartialEq)]
pub enum EmbedError {
    /// Ollama is not running or not reachable.
    Unavailable(String),
    /// The embedding model is not installed.
    ModelMissing(String),
    /// Ollama refused the request (for example a model without embedding support).
    Rejected(String),
    /// The response was malformed or had inconsistent dimensions.
    Invalid(String),
}
impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "Ollama is not reachable for embeddings ({e})"),
            Self::ModelMissing(m) => write!(
                f,
                "embedding model \"{m}\" is not installed; run `ollama pull {m}` or choose another model"
            ),
            Self::Rejected(e) => write!(f, "embedding request was rejected ({e})"),
            Self::Invalid(e) => write!(f, "invalid embedding response: {e}"),
        }
    }
}
impl std::error::Error for EmbedError {}

pub fn embed(model: &str, text: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
    let response = engine::client()
        .map_err(|e| EmbedError::Unavailable(e.to_string()))?
        .post(format!("{}/api/embed", engine::OLLAMA))
        .json(&json!({"model":model,"input":text,"truncate":true}))
        .send()
        .map_err(|e| EmbedError::Unavailable(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        let body: String = response
            .text()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        return Err(
            if status == reqwest::StatusCode::NOT_FOUND || body.to_lowercase().contains("not found")
            {
                EmbedError::ModelMissing(model.to_owned())
            } else {
                EmbedError::Rejected(format!("HTTP {status}: {}", body.trim()))
            },
        );
    }
    let value: Value = response
        .json()
        .map_err(|e| EmbedError::Invalid(e.to_string()))?;
    let vectors: Vec<Vec<f32>> = serde_json::from_value(value["embeddings"].clone())
        .map_err(|e| EmbedError::Invalid(e.to_string()))?;
    validate(&vectors, text.len())?;
    Ok(vectors)
}
fn validate(vectors: &[Vec<f32>], expected: usize) -> Result<(), EmbedError> {
    let invalid = |m: &str| Err(EmbedError::Invalid(m.to_owned()));
    if vectors.len() != expected {
        return invalid("embedding count mismatch");
    }
    if !vectors
        .iter()
        .all(|v| !v.is_empty() && v.iter().all(|f| f.is_finite()))
    {
        return invalid("empty or non-finite vector");
    }
    if vectors.iter().any(|v| v.len() != vectors[0].len()) {
        return invalid("inconsistent vector dimensions");
    }
    Ok(())
}

// Rerank a bounded lexical shortlist, caching vectors on disk. This avoids embedding
// the whole 100k-file library before the first question can be answered. Lexical and
// vector ranks are combined with reciprocal-rank fusion; at most two passages per file.
// Errors are `EmbedError`s (or cache errors); see `rerank_or_lexical` for a fallback.
pub fn rerank(
    db: &Path,
    model: &str,
    query: &str,
    candidates: Vec<Source>,
    limit: usize,
) -> Result<Vec<Source>> {
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let query_vector = embed(model, &[format!("search_query: {query}")])?.remove(0);
    let conn = Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS vectors(model TEXT, text TEXT, vector TEXT, PRIMARY KEY(model,text));")?;
    // Cached vectors of another size come from a model that changed; embed them again.
    let mut vectors: Vec<Option<Vec<f32>>> = candidates
        .iter()
        .map(|s| {
            conn.query_row(
                "SELECT vector FROM vectors WHERE model=?1 AND text=?2",
                params![model, s.text],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| serde_json::from_str::<Vec<f32>>(&v).ok())
            .filter(|v| v.len() == query_vector.len())
        })
        .collect();
    let missing: Vec<usize> = (0..vectors.len())
        .filter(|&i| vectors[i].is_none())
        .collect();
    if !missing.is_empty() {
        let inputs: Vec<String> = missing
            .iter()
            .map(|&i| format!("search_document: {}", candidates[i].text))
            .collect();
        for (&i, vector) in missing.iter().zip(embed(model, &inputs)?) {
            if vector.len() != query_vector.len() {
                return Err(
                    EmbedError::Invalid("query and passage dimensions differ".into()).into(),
                );
            }
            conn.execute(
                "INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)",
                params![model, candidates[i].text, serde_json::to_string(&vector)?],
            )?;
            vectors[i] = Some(vector);
        }
    }
    let similarities: Vec<f32> = vectors
        .iter()
        .map(|v| {
            v.as_deref()
                .and_then(|v| cosine(&query_vector, v))
                .unwrap_or(-1.0)
        })
        .collect();
    let order = fuse(&similarities);
    Ok(limit_per_file(
        order.into_iter().map(|i| candidates[i].clone()),
        PER_FILE,
        limit,
    ))
}

/// `rerank`, or the lexical order when embeddings fail. The note explains the fallback
/// and is meant for an Activity line.
pub fn rerank_or_lexical(
    db: &Path,
    model: &str,
    query: &str,
    candidates: Vec<Source>,
    limit: usize,
) -> (Vec<Source>, Option<String>) {
    match rerank(db, model, query, candidates.clone(), limit) {
        Ok(sources) => (sources, None),
        Err(e) => (
            limit_per_file(candidates, PER_FILE, limit),
            Some(format!(
                "Embedding rerank unavailable, using keyword results: {e:#}"
            )),
        ),
    }
}

const RRF_K: f32 = 60.0;
// Candidate indices ordered by reciprocal-rank fusion of the lexical order (index)
// and the vector order (descending similarity). Ties keep the lexical order.
fn fuse(similarities: &[f32]) -> Vec<usize> {
    let mut by_vector: Vec<usize> = (0..similarities.len()).collect();
    by_vector.sort_by(|&a, &b| similarities[b].total_cmp(&similarities[a]));
    let mut score: Vec<f32> = (0..similarities.len())
        .map(|lexical| 1.0 / (RRF_K + lexical as f32 + 1.0))
        .collect();
    for (rank, &i) in by_vector.iter().enumerate() {
        score[i] += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    let mut order: Vec<usize> = (0..similarities.len()).collect();
    order.sort_by(|&a, &b| score[b].total_cmp(&score[a]).then(a.cmp(&b)));
    order
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let dot: f32 = a.iter().zip(b).map(|(a, b)| a * b).sum();
    let norm = (a.iter().map(|a| a * a).sum::<f32>() * b.iter().map(|b| b * b).sum::<f32>()).sqrt();
    (norm > 0.0 && norm.is_finite()).then_some(dot / norm)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cosine_handles_dimensions() {
        assert!((cosine(&[1., 0.], &[1., 0.]).unwrap() - 1.).abs() < 0.001);
        assert!(cosine(&[1.], &[1., 0.]).is_none());
        assert!(cosine(&[0.], &[0.]).is_none());
    }
    #[test]
    fn fusion_combines_both_ranks() {
        // Lexical order is 0,1,2. A passage ranked second lexically but first by
        // vector overtakes one ranked first lexically but last by vector.
        assert_eq!(fuse(&[0.1, 0.9, 0.5]), [1, 0, 2]);
        // Mirror-image ranks tie, and ties keep the lexical order.
        assert_eq!(fuse(&[0.1, 0.5, 0.9]), [0, 2, 1]);
        assert_eq!(fuse(&[0.5, 0.5]), [0, 1]);
    }
    #[test]
    fn validation_reports_clear_errors() {
        assert!(validate(&[vec![1.0f32], vec![1.0]], 2).is_ok());
        assert_eq!(
            validate(&[vec![1.0f32]], 2),
            Err(EmbedError::Invalid("embedding count mismatch".into()))
        );
        assert!(validate(&[vec![1.0f32], vec![1.0, 2.0]], 2).is_err());
        assert!(validate(&[vec![f32::NAN]], 1).is_err());
        let missing = EmbedError::ModelMissing("nomic-embed-text".into()).to_string();
        assert!(missing.contains("ollama pull nomic-embed-text"));
    }
}

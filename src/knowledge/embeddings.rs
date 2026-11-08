use crate::{docs::Source, engine};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::Path;

pub fn embed(model: &str, text: &[String]) -> Result<Vec<Vec<f32>>> {
    let value: Value = engine::client()?
        .post(format!("{}/api/embed", engine::OLLAMA))
        .json(&json!({"model":model,"input":text,"truncate":true}))
        .send()?
        .error_for_status()?
        .json()?;
    let vectors: Vec<Vec<f32>> = serde_json::from_value(value["embeddings"].clone())
        .context("Invalid embedding response")?;
    ensure!(vectors.len() == text.len(), "Embedding count mismatch");
    ensure!(
        vectors
            .iter()
            .all(|v| !v.is_empty() && v.iter().all(|f| f.is_finite())),
        "Invalid embedding vector"
    );
    Ok(vectors)
}

// Rerank a bounded lexical shortlist, caching vectors on disk. This avoids embedding
// the whole 100k-file library before the first question can be answered.
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
    let conn = Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS vectors(model TEXT, text TEXT, vector TEXT, PRIMARY KEY(model,text));")?;
    let mut vectors: Vec<Option<Vec<f32>>> = Vec::new();
    let mut missing = Vec::new();
    let mut positions = Vec::new();
    for (i, s) in candidates.iter().enumerate() {
        let cached = conn
            .query_row(
                "SELECT vector FROM vectors WHERE model=?1 AND text=?2",
                params![model, s.text],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        if cached.is_none() {
            missing.push(format!("search_document: {}", s.text));
            positions.push(i);
        }
        vectors.push(cached);
    }
    if !missing.is_empty() {
        let generated = embed(model, &missing)?;
        for (position, vector) in positions.into_iter().zip(generated) {
            conn.execute(
                "INSERT OR REPLACE INTO vectors VALUES(?1,?2,?3)",
                params![
                    model,
                    candidates[position].text,
                    serde_json::to_string(&vector)?
                ],
            )?;
            vectors[position] = Some(vector);
        }
    }
    let query_vector = embed(model, &[format!("search_query: {query}")])?.remove(0);
    let mut scored = candidates
        .into_iter()
        .zip(vectors)
        .map(|(s, v)| {
            Ok((
                cosine(&query_vector, &v.context("Missing cached vector")?)?,
                s,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    Ok(scored.into_iter().take(limit).map(|(_, s)| s).collect())
}

fn cosine(a: &[f32], b: &[f32]) -> Result<f32> {
    ensure!(
        a.len() == b.len() && !a.is_empty(),
        "Embedding dimensions changed; choose a different model or remove its cache"
    );
    let dot: f32 = a.iter().zip(b).map(|(a, b)| a * b).sum();
    let norm = (a.iter().map(|a| a * a).sum::<f32>() * b.iter().map(|b| b * b).sum::<f32>()).sqrt();
    ensure!(
        norm > 0.0 && norm.is_finite(),
        "Zero or invalid embedding norm"
    );
    Ok(dot / norm)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cosine_handles_dimensions() {
        assert!((cosine(&[1., 0.], &[1., 0.]).unwrap() - 1.).abs() < 0.001);
        assert!(cosine(&[1.], &[1., 0.]).is_err());
        assert!(cosine(&[0.], &[0.]).is_err());
    }
}

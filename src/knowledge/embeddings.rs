//! Embedding rerank of the lexical (BM25) shortlist, with vectors cached in
//! SQLite. Embeddings come from llama.cpp's `llama-server --embedding` (the
//! shared `llama::EmbedRuntime`, started on demand) or from Ollama, per
//! `config.embedding_backend`.
use crate::{
    config::Config,
    core::{llama, setup},
    docs::{PER_FILE, Source, limit_per_file},
    engine,
};
use anyhow::Result;
use reqwest::blocking::Client;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Why embedding failed, so callers can fall back to lexical (BM25) results.
/// Wrapped in `anyhow::Error` by `rerank`; use `err.downcast_ref::<EmbedError>()`.
#[derive(Clone, Debug, PartialEq)]
pub enum EmbedError {
    /// The embedding server (llama.cpp or Ollama) is not running or not reachable.
    Unavailable(String),
    /// The embedding model is not installed (an Ollama name or a GGUF file name).
    ModelMissing(String),
    /// The server refused the request (for example a model without embedding support).
    Rejected(String),
    /// The response was malformed or had inconsistent dimensions.
    Invalid(String),
}
impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(e) => write!(f, "the embedding server is not reachable ({e})"),
            Self::ModelMissing(m) if m.ends_with(".gguf") => write!(
                f,
                "embedding model {m} is not downloaded; download it on the Models page"
            ),
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

/// Most vectors kept in the cache; the least recently used go first.
const CACHE_CAP: i64 = 20_000;
/// Longest input sent to the llama.cpp server (its batch is 2048 tokens).
const MAX_INPUT_CHARS: usize = 6000;

fn client() -> Result<Client, EmbedError> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    llama::shared(&CLIENT, || {
        Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(300))
            .build()
    })
    .map_err(|e| EmbedError::Unavailable(e.to_string()))
}

fn uses_ollama(config: &Config) -> bool {
    config.embedding_backend.eq_ignore_ascii_case("ollama")
}

/// The GGUF file name the llama.cpp backend embeds with (the default when none
/// is installed yet).
fn gguf_name(config: &Config) -> String {
    llama::embed_model_path(config)
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| setup::EMBED_MODEL.file.into())
}

/// Cache key of the backend and model that produced a vector ("ollama:<tag>"
/// or "gguf:<file>"), so switching either never mixes vectors.
fn cache_model(config: &Config) -> String {
    if uses_ollama(config) {
        return format!("ollama:{}", config.embedding_model.trim());
    }
    format!("gguf:{}", gguf_name(config))
}

/// How a model wants its inputs marked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prompting {
    /// nomic-embed-text: "search_query: " and "search_document: " (also the
    /// fallback for unknown models, as before).
    Nomic,
    /// Qwen3-Embedding: an instruction on the query only, passages as they are.
    Qwen3,
}

const QWEN3_QUERY: &str = "Instruct: Given a search query, retrieve relevant passages\nQuery: ";

/// The prompting of a model, from its GGUF file name or Ollama tag.
fn prompting_for(model: &str) -> Prompting {
    if model.to_lowercase().contains("qwen3-embedding") {
        Prompting::Qwen3
    } else {
        Prompting::Nomic
    }
}

fn prompting(config: &Config) -> Prompting {
    if uses_ollama(config) {
        prompting_for(&config.embedding_model)
    } else {
        prompting_for(&gguf_name(config))
    }
}

impl Prompting {
    fn query(self, query: &str) -> String {
        match self {
            Self::Nomic => format!("search_query: {query}"),
            Self::Qwen3 => format!("{QWEN3_QUERY}{query}"),
        }
    }

    fn passage(self, text: &str) -> String {
        match self {
            Self::Nomic => format!("search_document: {text}"),
            Self::Qwen3 => text.to_string(),
        }
    }
}

/// Embeds `text` with the configured backend: "ollama", or llama.cpp otherwise
/// (the default "llama"), starting the embedding server when needed. Blocking.
pub fn embed(config: &Config, text: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
    let vectors = if uses_ollama(config) {
        embed_ollama(&config.embedding_model, text)?
    } else {
        embed_llama(config, text)?
    };
    validate(&vectors, text.len())?;
    Ok(vectors)
}

/// OpenAI /v1/embeddings on the shared llama.cpp embedding server.
fn embed_llama(config: &Config, text: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
    if llama::embed_model_path(config).is_none() {
        let wanted = config
            .embedding_gguf
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| setup::EMBED_MODEL.file.into());
        return Err(EmbedError::ModelMissing(wanted));
    }
    let runtime = llama::embed_runtime();
    let port = match runtime.port() {
        Some(port) => port,
        None => runtime
            .start(config)
            .map_err(|e| EmbedError::Unavailable(format!("{e:#}")))?,
    };
    let input: Vec<String> = text
        .iter()
        .map(|t| t.chars().take(MAX_INPUT_CHARS).collect())
        .collect();
    let response = client()?
        .post(format!("http://127.0.0.1:{port}/v1/embeddings"))
        .json(&json!({"model": llama::EMBED_ALIAS, "input": input}))
        .send()
        .map_err(|e| EmbedError::Unavailable(e.to_string()))?;
    let value = read_response(response, llama::EMBED_ALIAS)?;
    parse_openai(&value).ok_or_else(|| EmbedError::Invalid("no data[].embedding".into()))
}

fn embed_ollama(model: &str, text: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
    let response = client()?
        .post(format!("{}/api/embed", engine::OLLAMA))
        .json(&json!({"model":model,"input":text,"truncate":true}))
        .send()
        .map_err(|e| EmbedError::Unavailable(format!("Ollama: {e}")))?;
    let value = read_response(response, model)?;
    serde_json::from_value(value["embeddings"].clone())
        .map_err(|e| EmbedError::Invalid(e.to_string()))
}

/// Client for `ollama pull`: no total timeout (a model is gigabytes).
fn pull_client() -> Result<Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    llama::shared(&CLIENT, || {
        Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(3))
            .timeout(None::<Duration>)
            .build()
    })
}

/// One line of Ollama's streamed /api/pull answer.
#[derive(Debug, PartialEq)]
enum PullLine {
    Progress {
        status: String,
        done: u64,
        total: u64,
    },
    Success,
    Failed(String),
}

fn parse_pull_line(line: &str) -> Option<PullLine> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if let Some(error) = v["error"].as_str() {
        return Some(PullLine::Failed(error.to_string()));
    }
    let status = v["status"].as_str()?.to_string();
    if status == "success" {
        return Some(PullLine::Success);
    }
    Some(PullLine::Progress {
        status,
        done: v["completed"].as_u64().unwrap_or(0),
        total: v["total"].as_u64().unwrap_or(0),
    })
}

/// Pulls an Ollama model (`ollama pull <tag>`) through POST {OLLAMA}/api/pull,
/// streaming its progress into `progress` (stage, done, total). Blocking: run
/// it on a worker thread; `cancel` stops it (Ollama resumes the download next
/// time). `progress.finished` is set at the end, with `error` on failure.
pub fn ollama_pull(
    tag: &str,
    progress: std::sync::Arc<Mutex<setup::SetupProgress>>,
    cancel: std::sync::Arc<AtomicBool>,
) -> Result<()> {
    let result = run_pull(tag, &progress, &cancel);
    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
    p.finished = true;
    match &result {
        Ok(()) => {
            p.stage = format!("Installed {tag}");
            p.done = p.total;
        }
        Err(e) => p.error = Some(format!("{e:#}")),
    }
    result
}

fn run_pull(tag: &str, progress: &Mutex<setup::SetupProgress>, cancel: &AtomicBool) -> Result<()> {
    use std::io::BufRead;
    let tag = tag.trim();
    anyhow::ensure!(
        !tag.is_empty() && !tag.chars().any(char::is_whitespace),
        "Not an Ollama model name: {tag:?}"
    );
    {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        p.stage = format!("Pulling {tag}");
        (p.done, p.total) = (0, 0);
    }
    let response = pull_client()?
        .post(format!("{}/api/pull", engine::OLLAMA))
        .json(&json!({"model": tag, "stream": true}))
        .send()
        .map_err(|e| anyhow::anyhow!("Ollama is not running ({e})"))?;
    let status = response.status();
    if !status.is_success() {
        let body: String = response
            .text()
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        anyhow::bail!(
            "Ollama refused to pull {tag} (HTTP {status}): {}",
            body.trim()
        );
    }
    for line in std::io::BufReader::new(response).lines() {
        anyhow::ensure!(!cancel.load(Ordering::Relaxed), "Cancelled");
        let line = line.map_err(|e| anyhow::anyhow!("Pull of {tag} interrupted ({e})"))?;
        match parse_pull_line(&line) {
            Some(PullLine::Success) => return Ok(()),
            Some(PullLine::Failed(e)) => anyhow::bail!("Ollama could not pull {tag}: {e}"),
            Some(PullLine::Progress {
                status,
                done,
                total,
            }) => {
                let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                p.stage = format!("{tag}: {status}");
                (p.done, p.total) = (done, total);
            }
            None => {}
        }
    }
    anyhow::bail!("Ollama stopped before {tag} was pulled")
}

/// Ollama names are equal with or without the ":latest" tag.
fn same_ollama_model(a: &str, b: &str) -> bool {
    let plain = |t: &str| {
        let t = t.trim().to_ascii_lowercase();
        t.strip_suffix(":latest").map(str::to_string).unwrap_or(t)
    };
    plain(a) == plain(b)
}

/// True when Ollama has `tag` installed (GET /api/tags). Blocking, short
/// timeout; false when Ollama is not running.
// The UI reads its cached Ollama list instead; this one is for worker threads.
#[allow(dead_code)]
pub fn ollama_has_model(tag: &str) -> bool {
    let Ok(client) = client() else {
        return false;
    };
    client
        .get(format!("{}/api/tags", engine::OLLAMA))
        .timeout(Duration::from_secs(3))
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Value>().ok())
        .is_some_and(|v| lists_ollama_model(&v, tag))
}

fn lists_ollama_model(tags: &Value, tag: &str) -> bool {
    tags["models"].as_array().is_some_and(|models| {
        models.iter().any(|m| {
            ["name", "model"]
                .iter()
                .filter_map(|k| m[*k].as_str())
                .any(|name| same_ollama_model(name, tag))
        })
    })
}

/// Whether Embedding rerank should switch itself on: the user never toggled
/// it (`user_set` is config.rerank_user_set), it is off, an embedding model is
/// ready (`model_ready`: llama::embed_model_path(config).is_some() for the
/// llama backend, ollama_has_model(&config.embedding_model) for Ollama), and
/// at least one documentation source is enabled.
pub fn should_auto_enable(config: &Config, user_set: bool, model_ready: bool) -> bool {
    !user_set
        && !config.use_embeddings
        && model_ready
        && config.doc_sources.iter().any(|s| s.enabled)
}

fn read_response(response: reqwest::blocking::Response, model: &str) -> Result<Value, EmbedError> {
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
    response
        .json()
        .map_err(|e| EmbedError::Invalid(e.to_string()))
}

/// Vectors of an OpenAI embeddings response, in input order.
fn parse_openai(value: &Value) -> Option<Vec<Vec<f32>>> {
    let mut items: Vec<(u64, Vec<f32>)> = value["data"]
        .as_array()?
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let index = item["index"].as_u64().unwrap_or(i as u64);
            serde_json::from_value(item["embedding"].clone())
                .ok()
                .map(|v| (index, v))
        })
        .collect::<Option<_>>()?;
    items.sort_by_key(|(i, _)| *i);
    Some(items.into_iter().map(|(_, v)| v).collect())
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

/// Little-endian f32s, 4 bytes each (a third of the JSON size, and no parsing).
fn encode(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn decode(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

/// Cache key of a passage: a stable 64-bit hash, so the text is not stored again.
fn text_key(text: &str) -> i64 {
    llama::fnv1a(text.as_bytes()) as i64
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Opens the vector cache in WAL mode. The old JSON `vectors` table is only a
/// cache, so it is dropped rather than migrated.
fn open_cache(db: &Path) -> Result<Connection> {
    let conn = Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS vectors2(model TEXT NOT NULL, key INTEGER NOT NULL, vector BLOB NOT NULL, used INTEGER NOT NULL, PRIMARY KEY(model, key));
         CREATE INDEX IF NOT EXISTS vectors2_used ON vectors2(used);
         DROP TABLE IF EXISTS vectors;",
    )?;
    Ok(conn)
}

/// Runs `f` on the vector cache connection, opened once per path and kept, so each docs
/// question skips the open and schema setup and keeps its prepared statements. A connection
/// that fails is closed and reopened next time.
fn with_cache<T>(db: &Path, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    static CACHE: Mutex<Option<(PathBuf, Connection)>> = Mutex::new(None);
    let mut slot = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_none_or(|(path, _)| path != db) {
        *slot = None;
        *slot = Some((db.to_path_buf(), open_cache(db)?));
    }
    let result = slot
        .as_mut()
        .map(|(_, conn)| f(conn))
        .expect("cache connection");
    if result.is_err() {
        *slot = None;
    }
    result
}

/// Starts the llama.cpp embedding server in the background when rerank will need it, so the
/// first documentation question does not wait for the model to load. Does nothing for Ollama,
/// without an embedding model, or while the server already runs; a failed start is retried
/// (and reported) by the first rerank.
pub fn prewarm(config: &Config) {
    static STARTING: AtomicBool = AtomicBool::new(false);
    if uses_ollama(config)
        || llama::embed_model_path(config).is_none()
        || llama::embed_runtime().port().is_some()
        || STARTING.swap(true, Ordering::AcqRel)
    {
        return;
    }
    let config = config.clone();
    std::thread::spawn(move || {
        let _ = llama::embed_runtime().start(&config);
        STARTING.store(false, Ordering::Release);
    });
}

// Rerank a bounded lexical shortlist, caching vectors on disk. This avoids embedding
// the whole 100k-file library before the first question can be answered. Lexical and
// vector ranks are combined with reciprocal-rank fusion; at most two passages per file.
// Errors are `EmbedError`s (or cache errors); see `rerank_or_lexical` for a fallback.
pub fn rerank(
    db: &Path,
    config: &Config,
    query: &str,
    candidates: Vec<Source>,
    limit: usize,
) -> Result<Vec<Source>> {
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let model = cache_model(config);
    let mut vectors: Vec<Option<Vec<f32>>> = with_cache(db, |conn| {
        let mut get =
            conn.prepare_cached("SELECT vector FROM vectors2 WHERE model=?1 AND key=?2")?;
        Ok(candidates
            .iter()
            .map(|s| {
                get.query_row(params![model, text_key(&s.text)], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .ok()
                .map(|b| decode(&b))
            })
            .collect())
    })?;
    let style = prompting(config);
    let document = |i: usize| style.passage(&candidates[i].text);
    // The query and the passages missing from the cache go in one embedding request.
    let mut missing: Vec<usize> = (0..vectors.len())
        .filter(|&i| vectors[i].is_none())
        .collect();
    let mut inputs = vec![style.query(query)];
    inputs.extend(missing.iter().map(|&i| document(i)));
    let mut fresh = embed(config, &inputs)?;
    let query_vector = fresh.remove(0);
    // Cached vectors of another size come from a model that changed; embed them again.
    let stale: Vec<usize> = (0..vectors.len())
        .filter(|&i| {
            vectors[i]
                .as_ref()
                .is_some_and(|v| v.len() != query_vector.len())
        })
        .collect();
    if !stale.is_empty() {
        let inputs: Vec<String> = stale.iter().map(|&i| document(i)).collect();
        fresh.extend(embed(config, &inputs)?);
        for &i in &stale {
            vectors[i] = None;
        }
        missing.extend(stale);
    }
    if fresh.iter().any(|v| v.len() != query_vector.len()) {
        return Err(EmbedError::Invalid("query and passage dimensions differ".into()).into());
    }
    // One transaction (one sync): hits are marked used, new vectors stored, the oldest pruned.
    let now = unix_now();
    with_cache(db, |conn| {
        let tx = conn.transaction()?;
        {
            let mut touch =
                tx.prepare_cached("UPDATE vectors2 SET used=?3 WHERE model=?1 AND key=?2")?;
            for (s, v) in candidates.iter().zip(&vectors) {
                if v.is_some() {
                    touch.execute(params![model, text_key(&s.text), now])?;
                }
            }
            let mut put =
                tx.prepare_cached("INSERT OR REPLACE INTO vectors2 VALUES(?1,?2,?3,?4)")?;
            for (&i, vector) in missing.iter().zip(&fresh) {
                put.execute(params![
                    model,
                    text_key(&candidates[i].text),
                    encode(vector),
                    now
                ])?;
            }
            if !fresh.is_empty() {
                tx.execute(
                    "DELETE FROM vectors2 WHERE rowid IN (SELECT rowid FROM vectors2 ORDER BY used LIMIT max(0, (SELECT count(*) FROM vectors2) - ?1))",
                    params![CACHE_CAP],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    for (&i, vector) in missing.iter().zip(fresh) {
        vectors[i] = Some(vector);
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
    config: &Config,
    query: &str,
    candidates: Vec<Source>,
    limit: usize,
) -> (Vec<Source>, Option<String>) {
    match rerank(db, config, query, candidates.clone(), limit) {
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
        let gguf = EmbedError::ModelMissing("nomic.gguf".into()).to_string();
        assert!(gguf.contains("Models page") && !gguf.contains("ollama"));
    }
    #[test]
    fn vectors_round_trip_as_bytes() {
        let v = vec![1.5f32, -0.25, 0.0, f32::MAX, 1e-7];
        let bytes = encode(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        assert_eq!(decode(&bytes), v);
        assert!(decode(&[1, 2, 3]).is_empty());
    }
    #[test]
    fn reads_openai_embeddings_in_input_order() {
        let v = json!({"data":[
            {"index":1,"embedding":[0.0,1.0]},
            {"index":0,"embedding":[1.0,0.0]}
        ]});
        assert_eq!(parse_openai(&v), Some(vec![vec![1.0, 0.0], vec![0.0, 1.0]]));
        assert_eq!(parse_openai(&json!({"data":[{"embedding":"x"}]})), None);
        assert_eq!(parse_openai(&json!({"error":"x"})), None);
    }
    #[test]
    fn cache_keys_name_backend_and_model() {
        let mut c = Config {
            embedding_backend: "ollama".into(),
            embedding_model: "qwen3-embedding:4b".into(),
            ..Default::default()
        };
        assert_eq!(cache_model(&c), "ollama:qwen3-embedding:4b");
        c.embedding_model = "nomic-embed-text".into();
        assert_eq!(cache_model(&c), "ollama:nomic-embed-text");
        c.embedding_backend = "llama".into();
        assert!(cache_model(&c).starts_with("gguf:"));
    }
    #[test]
    fn qwen3_marks_only_the_query() {
        let qwen = prompting_for("Qwen3-Embedding-4B-Q4_K_M.gguf");
        assert_eq!(qwen, Prompting::Qwen3);
        assert_eq!(prompting_for("qwen3-embedding:4b"), Prompting::Qwen3);
        assert_eq!(
            qwen.query("how to x"),
            "Instruct: Given a search query, retrieve relevant passages\nQuery: how to x"
        );
        assert_eq!(qwen.passage("text"), "text");
        let nomic = prompting_for("nomic-embed-text-v1.5.Q8_0.gguf");
        assert_eq!(nomic.query("q"), "search_query: q");
        assert_eq!(nomic.passage("p"), "search_document: p");
        let ollama_qwen = Config {
            embedding_backend: "Ollama".into(),
            embedding_model: "qwen3-embedding:4b".into(),
            ..Default::default()
        };
        assert_eq!(prompting(&ollama_qwen), Prompting::Qwen3);
    }
    #[test]
    fn reads_ollama_pull_stream() {
        assert_eq!(
            parse_pull_line(
                r#"{"status":"pulling abc","digest":"sha256:1","total":100,"completed":40}"#
            ),
            Some(PullLine::Progress {
                status: "pulling abc".into(),
                done: 40,
                total: 100
            })
        );
        assert_eq!(
            parse_pull_line(r#"{"status":"pulling manifest"}"#),
            Some(PullLine::Progress {
                status: "pulling manifest".into(),
                done: 0,
                total: 0
            })
        );
        assert_eq!(
            parse_pull_line(r#"{"status":"success"}"#),
            Some(PullLine::Success)
        );
        assert_eq!(
            parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#),
            Some(PullLine::Failed(
                "pull model manifest: file does not exist".into()
            ))
        );
        assert_eq!(parse_pull_line(""), None);
        assert_eq!(parse_pull_line("{}"), None);
    }
    #[test]
    fn finds_installed_ollama_models() {
        let tags = json!({"models":[
            {"name":"nomic-embed-text:latest","model":"nomic-embed-text:latest"},
            {"name":"qwen3-embedding:4b"}
        ]});
        assert!(lists_ollama_model(&tags, "nomic-embed-text"));
        assert!(lists_ollama_model(&tags, "Qwen3-Embedding:4b"));
        assert!(!lists_ollama_model(&tags, "qwen3-embedding:8b"));
        assert!(!lists_ollama_model(&json!({"error":"x"}), "a"));
    }
    #[test]
    fn rerank_switches_on_only_when_useful() {
        use crate::docs::{DocSource, SourceMode};
        let source = |enabled| DocSource {
            id: "1".into(),
            name: "docs".into(),
            path: PathBuf::from("/docs"),
            mode: SourceMode::Linked,
            enabled,
        };
        let mut c = Config {
            doc_sources: vec![source(false), source(true)],
            ..Default::default()
        };
        assert!(should_auto_enable(&c, false, true));
        assert!(!should_auto_enable(&c, true, true));
        assert!(!should_auto_enable(&c, false, false));
        c.doc_sources = vec![source(false)];
        assert!(!should_auto_enable(&c, false, true));
        c.doc_sources = vec![source(true)];
        c.use_embeddings = true;
        assert!(!should_auto_enable(&c, false, true));
    }
    #[test]
    fn cache_stores_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_cache(&dir.path().join("e.sqlite")).unwrap();
        for i in 0..3i64 {
            conn.execute(
                "INSERT OR REPLACE INTO vectors2 VALUES('m',?1,?2,?1)",
                params![i, encode(&[i as f32])],
            )
            .unwrap();
        }
        conn.execute(
            "DELETE FROM vectors2 WHERE rowid IN (SELECT rowid FROM vectors2 ORDER BY used LIMIT max(0, (SELECT count(*) FROM vectors2) - ?1))",
            params![2i64],
        )
        .unwrap();
        let keys: Vec<i64> = conn
            .prepare("SELECT key FROM vectors2 ORDER BY key")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|k| k.unwrap())
            .collect();
        assert_eq!(keys, [1, 2]);
        assert_eq!(text_key("a"), text_key("a"));
        assert_ne!(text_key("a"), text_key("b"));
    }
}

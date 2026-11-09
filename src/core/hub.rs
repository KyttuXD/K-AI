//! Hugging Face GGUF browser: searches repositories, lists their GGUF files
//! with the LFS SHA-256 used to verify downloads, and manages the models
//! downloaded into `setup::models_dir()`.
use super::{llama, setup};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};

const HUB: &str = "https://huggingface.co";

#[derive(Clone, Debug, PartialEq)]
pub struct HubModel {
    pub repo: String,
    pub downloads: u64,
    pub likes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HubFile {
    pub repo: String,
    /// Path inside the repository, e.g. "Qwen2.5-3B-Q4_K_M.gguf".
    pub file: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub url: String,
}

/// How a model of a given size fits this machine's RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitHint {
    Good,
    Tight,
    TooBig,
}

fn client() -> Result<Client> {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    llama::shared(&CLIENT, || {
        Client::builder()
            .user_agent("kyttu-ai")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
    })
}

fn get_json(request: reqwest::blocking::RequestBuilder) -> Result<Value> {
    let response = request.send().context("Could not reach Hugging Face")?;
    let status = response.status();
    ensure!(status.is_success(), "Hugging Face answered {status}");
    Ok(response.json()?)
}

/// GGUF repositories matching `query`, most downloaded first (at most 30).
pub fn search(query: &str) -> Result<Vec<HubModel>> {
    let request = client()?.get(format!("{HUB}/api/models")).query(&[
        ("search", query.trim()),
        ("filter", "gguf"),
        ("sort", "downloads"),
        ("limit", "30"),
    ]);
    Ok(parse_models(&get_json(request)?))
}

fn parse_models(v: &Value) -> Vec<HubModel> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let repo = m["id"].as_str().or_else(|| m["modelId"].as_str())?;
            Some(HubModel {
                repo: repo.to_string(),
                downloads: m["downloads"].as_u64().unwrap_or(0),
                likes: m["likes"].as_u64().unwrap_or(0),
            })
        })
        .collect()
}

/// "owner/name" with the characters Hugging Face allows.
fn valid_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    };
    repo.split_once('/')
        .is_some_and(|(owner, name)| ok(owner) && ok(name))
}

/// The GGUF files of a repository, largest quant last, with the SHA-256 that
/// Hugging Face stores for LFS files.
pub fn files(repo: &str) -> Result<Vec<HubFile>> {
    ensure!(valid_repo(repo), "Not a Hugging Face repository: {repo}");
    let request = client()?
        .get(format!("{HUB}/api/models/{repo}/tree/main"))
        .query(&[("recursive", "true")]);
    Ok(parse_files(repo, &get_json(request)?))
}

fn parse_files(repo: &str, v: &Value) -> Vec<HubFile> {
    let mut files: Vec<HubFile> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f["type"].as_str() == Some("file"))
        .filter_map(|f| {
            let path = f["path"].as_str()?;
            if !path.to_lowercase().ends_with(".gguf") || path.split('/').any(|s| s == "..") {
                return None;
            }
            let sha256 = f["lfs"]["oid"]
                .as_str()
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_ascii_lowercase);
            Some(HubFile {
                repo: repo.to_string(),
                file: path.to_string(),
                size: f["lfs"]["size"]
                    .as_u64()
                    .or_else(|| f["size"].as_u64())
                    .unwrap_or(0),
                sha256,
                url: format!("{HUB}/{repo}/resolve/main/{}", encode_path(path)),
            })
        })
        .collect();
    files.sort_by_key(|f| f.size);
    files
}

/// Percent-encodes a repository path, keeping the slashes.
fn encode_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// GGUF files in the models folder with their sizes, by name.
pub fn installed() -> Vec<(PathBuf, u64)> {
    let mut found: Vec<(PathBuf, u64)> = walkdir::WalkDir::new(setup::models_dir())
        .max_depth(3)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
        })
        .filter_map(|e| Some((e.path().to_path_buf(), e.metadata().ok()?.len())))
        .collect();
    found.sort_by_key(|(p, _)| p.file_name().map(|n| n.to_ascii_lowercase()));
    found
}

/// Deletes a downloaded model (and its partial download). Refuses anything
/// outside the models folder.
pub fn delete(path: &Path) -> Result<()> {
    let root = setup::models_dir()
        .canonicalize()
        .context("There is no models folder")?;
    let target = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    ensure!(
        target.starts_with(&root) && target != root && target.is_file(),
        "Only models in {} can be deleted here",
        root.display()
    );
    fs::remove_file(&target).with_context(|| format!("Could not delete {}", target.display()))?;
    let mut part = target.into_os_string();
    part.push(".part");
    let _ = fs::remove_file(part);
    Ok(())
}

/// How a model of `size` bytes fits this machine's RAM.
pub fn fits(size: u64) -> FitHint {
    fit_for(size, setup::total_ram_gb().map(|gb| gb << 30))
}

/// The weights plus ~20% for the KV cache and buffers plus 512 MiB for the
/// server; Good up to 60% of RAM (room for the OS and apps), Tight up to 85%.
fn fit_for(size: u64, ram: Option<u64>) -> FitHint {
    let Some(ram) = ram.filter(|&r| r > 0) else {
        return FitHint::Tight;
    };
    let need = size.saturating_add(size / 5).saturating_add(512 << 20);
    if need <= ram / 10 * 6 {
        FitHint::Good
    } else if need <= ram / 100 * 85 {
        FitHint::Tight
    } else {
        FitHint::TooBig
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn parses_search_results() {
        let v = json!([
            {"id":"bartowski/Qwen2.5-Coder-3B-Instruct-GGUF","downloads":1200,"likes":30},
            {"modelId":"a/b"},
            {"nothing":true}
        ]);
        let models = parse_models(&v);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].repo, "bartowski/Qwen2.5-Coder-3B-Instruct-GGUF");
        assert_eq!((models[0].downloads, models[0].likes), (1200, 30));
        assert_eq!(
            models[1],
            HubModel {
                repo: "a/b".into(),
                downloads: 0,
                likes: 0
            }
        );
        assert!(parse_models(&json!({"error":"x"})).is_empty());
    }
    #[test]
    fn lists_gguf_files_with_hashes() {
        let hash = "3e24342164b3d94991ba9692fdc0dd08e3fd7362e0aacc396a9a5c54a544c3b7";
        let v = json!([
            {"type":"file","path":"README.md","size":10},
            {"type":"directory","path":"q8"},
            {"type":"file","path":"q8/M Q8_0.gguf","size":134,"lfs":{"oid":hash,"size":2000}},
            {"type":"file","path":"m-Q4.GGUF","size":1000},
            {"type":"file","path":"../evil.gguf","size":1}
        ]);
        let files = parse_files("o/r", &v);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file, "m-Q4.GGUF");
        assert_eq!(files[0].sha256, None);
        assert_eq!(files[1].size, 2000);
        assert_eq!(files[1].sha256.as_deref(), Some(hash));
        assert_eq!(
            files[1].url,
            "https://huggingface.co/o/r/resolve/main/q8/M%20Q8_0.gguf"
        );
    }
    #[test]
    fn validates_repo_names() {
        assert!(valid_repo("nomic-ai/nomic-embed-text-v1.5-GGUF"));
        assert!(!valid_repo("nomic-ai"));
        assert!(!valid_repo("../x"));
        assert!(!valid_repo("a/b/c"));
        assert!(!valid_repo("a/b?x=1"));
    }
    #[test]
    fn fit_hints_scale_with_ram() {
        let gb = 1u64 << 30;
        assert_eq!(fit_for(2 * gb, Some(16 * gb)), FitHint::Good);
        assert_eq!(fit_for(5 * gb, Some(8 * gb)), FitHint::Tight);
        assert_eq!(fit_for(8 * gb, Some(8 * gb)), FitHint::TooBig);
        assert_eq!(fit_for(gb, None), FitHint::Tight);
        assert_eq!(fit_for(u64::MAX, Some(16 * gb)), FitHint::TooBig);
    }
    #[test]
    fn delete_stays_inside_the_models_folder() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("x.gguf");
        fs::write(&outside, b"x").unwrap();
        assert!(delete(&outside).is_err());
        assert!(outside.exists());
    }
}

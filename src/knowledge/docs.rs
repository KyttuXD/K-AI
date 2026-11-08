use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use walkdir::WalkDir;

#[derive(Clone, Default)]
pub struct Progress {
    pub running: bool,
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub line: usize,
    pub text: String,
}
#[derive(Clone)]
pub struct Library {
    pub db: PathBuf,
    pub progress: Arc<Mutex<Progress>>,
}

fn initialize(db: &Path) -> Result<Connection> {
    let conn = Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;
        CREATE VIRTUAL TABLE IF NOT EXISTS passages USING fts5(path UNINDEXED, line UNINDEXED, text, tokenize='porter unicode61');
        CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT);")?;
    Ok(conn)
}
impl Library {
    pub fn new(db: PathBuf) -> Self {
        let state = match initialize(&db).and_then(|c| {
            let files: usize =
                c.query_row("SELECT count(DISTINCT path) FROM passages", [], |r| {
                    r.get(0)
                })?;
            let chunks: usize = c.query_row("SELECT count(*) FROM passages", [], |r| r.get(0))?;
            Ok((files, chunks))
        }) {
            Ok((files, chunks)) => Progress {
                files,
                chunks,
                message: if chunks > 0 {
                    "Library ready"
                } else {
                    "Build an index to search your documentation"
                }
                .into(),
                ..Default::default()
            },
            Err(e) => Progress {
                message: format!("Index error: {e}"),
                ..Default::default()
            },
        };
        Self {
            db,
            progress: Arc::new(Mutex::new(state)),
        }
    }
    pub fn matches_root(&self, root: &Path) -> bool {
        let Ok(root) = root.canonicalize() else {
            return false;
        };
        initialize(&self.db)
            .ok()
            .and_then(|c| {
                c.query_row("SELECT value FROM metadata WHERE key='root'", [], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            })
            .is_some_and(|saved| saved == root.to_string_lossy())
    }
    pub fn rebuild(&self, root: PathBuf) {
        {
            let mut p = self.progress.lock().unwrap();
            if p.running {
                return;
            }
            *p = Progress {
                running: true,
                message: "Reading documentation…".into(),
                ..Default::default()
            };
        }
        let library = self.clone();
        std::thread::spawn(move || {
            let result = library.index(&root);
            let mut p = library.progress.lock().unwrap();
            p.running = false;
            p.message = match result {
                Ok(()) => "Library ready".into(),
                Err(e) => format!("Index failed: {e:#}"),
            };
        });
    }
    fn index(&self, root: &Path) -> Result<()> {
        let root = root
            .canonicalize()
            .context("Documentation folder does not exist")?;
        anyhow::ensure!(root.is_dir(), "Choose a folder");
        let mut conn = initialize(&self.db)?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM passages", [])?;
        let mut insert = tx.prepare("INSERT INTO passages(path,line,text) VALUES(?1,?2,?3)")?;
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                !matches!(
                    e.file_name().to_str(),
                    Some(".git" | "node_modules" | "target" | ".venv")
                )
            })
        {
            let Ok(entry) = entry else {
                self.progress.lock().unwrap().skipped += 1;
                continue;
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            if !matches!(
                ext.as_str(),
                "md" | "mdx" | "txt" | "rst" | "adoc" | "asc" | "html" | "htm"
            ) {
                continue;
            }
            let text = match read_document(path) {
                Ok(t) => t,
                Err(_) => {
                    self.progress.lock().unwrap().skipped += 1;
                    continue;
                }
            };
            let relative = path
                .strip_prefix(&root)?
                .to_string_lossy()
                .replace('\\', "/");
            let chunks = chunk_text(&text);
            for (line, text) in &chunks {
                insert.execute(params![relative, line, text])?;
            }
            let mut p = self.progress.lock().unwrap();
            p.files += 1;
            p.chunks += chunks.len();
            p.message = relative;
        }
        drop(insert);
        tx.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('root',?1)",
            [root.to_string_lossy().as_ref()],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn search(&self, query: &str, pack: &str, limit: usize) -> Result<Vec<Source>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let conn = initialize(&self.db)?;
        let mut stmt = conn.prepare("SELECT path,line,text FROM passages WHERE passages MATCH ?1 AND (?2='' OR substr(path,1,length(?2)+1)=?2||'/') ORDER BY bm25(passages) LIMIT ?3")?;
        let rows = stmt.query_map(params![terms, pack, (limit.clamp(1, 20) * 4) as i64], |r| {
            Ok(Source {
                path: r.get(0)?,
                line: r.get(1)?,
                text: r.get(2)?,
            })
        })?;
        let mut seen = HashSet::new();
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|s| seen.insert(s.path.clone()))
            .take(limit)
            .collect())
    }
}
pub fn read_document(path: &Path) -> Result<String> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= 2_000_000,
        "File exceeds 2 MB limit"
    );
    let raw = std::fs::read_to_string(path)?;
    if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("html" | "htm")
    ) {
        let html = Html::parse_document(&raw);
        let node = ["main", "article", ".document", "body"]
            .iter()
            .find_map(|selector| html.select(&Selector::parse(selector).unwrap()).next());
        Ok(node
            .map(|n| {
                n.text()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default())
    } else {
        Ok(raw)
    }
}
fn chunk_text(text: &str) -> Vec<(usize, String)> {
    let mut result = Vec::new();
    let (mut buffer, mut start) = (String::new(), 1);
    for (i, line) in text.lines().enumerate() {
        for segment in line.chars().collect::<Vec<_>>().chunks(1800) {
            if buffer.chars().count() + segment.len() > 1800 {
                let previous = std::mem::take(&mut buffer);
                if !previous.trim().is_empty() {
                    result.push((start, previous));
                }
                start = i + 1;
            }
            if buffer.is_empty() {
                start = i + 1;
            }
            buffer.extend(segment.iter());
            buffer.push('\n');
        }
    }
    if !buffer.trim().is_empty() {
        result.push((start, buffer));
    }
    result
}
fn search_terms(query: &str) -> String {
    const STOP: &[&str] = &[
        "the", "and", "how", "can", "what", "with", "this", "that", "for", "does", "from",
        "please", "you", "use", "using", "about", "are", "into", "would", "could", "should",
    ];
    let mut seen = HashSet::new();
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_lowercase)
        .filter(|s| s.len() > 1 && !STOP.contains(&s.as_str()) && seen.insert(s.clone()))
        .take(12)
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode() {
        let chunks = chunk_text(&format!("{}\nsecond", "é".repeat(1900)));
        assert!(chunks.len() >= 2);
        assert_eq!(
            chunks
                .iter()
                .map(|(_, s)| s.matches('é').count())
                .sum::<usize>(),
            1900
        );
        assert!(chunks.last().unwrap().1.contains("second"));
    }
    #[test]
    fn search_respects_pack_and_deduplicates_files() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("localai-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let library = Library::new(dir.join("test.sqlite"));
        let c = initialize(&library.db)?;
        c.execute_batch("DELETE FROM passages; INSERT INTO passages VALUES('rust/a.md',1,'ownership borrow reference'); INSERT INTO passages VALUES('rust/a.md',30,'ownership borrow'); INSERT INTO passages VALUES('js/b.md',1,'ownership');")?;
        assert_eq!(library.search("ownership", "rust", 4)?.len(), 1);
        assert_eq!(library.search("ownership", "js", 4)?[0].path, "js/b.md");
        drop(c);
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

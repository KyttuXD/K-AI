use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};
use walkdir::WalkDir;

// Bump when the passages/files layout changes: older databases are dropped once
// and rebuilt from scratch on the next index.
const SCHEMA: &str = "2";
const MAX_SKIPPED: usize = 200;
const CHUNK_CHARS: usize = 1800;
pub const PER_FILE: usize = 2;

#[derive(Clone, Default)]
pub struct Progress {
    pub running: bool,
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub message: String,
    /// (relative path, reason), capped at 200 entries; `skipped` keeps the full count.
    pub skipped_files: Vec<(String, String)>,
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
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT);",
    )?;
    let version: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key='schema'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if version.as_deref() != Some(SCHEMA) {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
            DROP TABLE IF EXISTS passages; DROP TABLE IF EXISTS files; DELETE FROM metadata;
            INSERT INTO metadata(key,value) VALUES('schema','{SCHEMA}');
            COMMIT;"
        ))?;
    }
    conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS passages USING fts5(path UNINDEXED, line UNINDEXED, section, text, tokenize='porter unicode61');
        CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, size INTEGER, mtime INTEGER, chunks INTEGER);")?;
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
                Ok(message) => message,
                Err(e) => format!("Index failed: {e:#}"),
            };
        });
    }
    fn skip(&self, path: impl Into<String>, reason: impl Into<String>) {
        let mut p = self.progress.lock().unwrap();
        p.skipped += 1;
        if p.skipped_files.len() < MAX_SKIPPED {
            p.skipped_files.push((path.into(), reason.into()));
        }
    }
    // Incremental: files whose path, size and mtime match the last run keep their
    // passages; changed and deleted files are removed before re-reading.
    fn index(&self, root: &Path) -> Result<String> {
        let root = root
            .canonicalize()
            .context("Documentation folder does not exist")?;
        anyhow::ensure!(root.is_dir(), "Choose a folder");
        let root_text = root.to_string_lossy().into_owned();
        let mut conn = initialize(&self.db)?;
        let tx = conn.transaction()?;
        let saved: Option<String> = tx
            .query_row("SELECT value FROM metadata WHERE key='root'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if saved.as_deref() != Some(root_text.as_str()) {
            tx.execute_batch("DELETE FROM passages; DELETE FROM files;")?;
        }
        let mut known: HashMap<String, (i64, i64, usize)> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT path,size,mtime,chunks FROM files")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, usize>(3)?,
                    ),
                ))
            })?;
            for row in rows {
                let (path, state) = row?;
                known.insert(path, state);
            }
        }
        let (mut pending, mut stale) = (Vec::new(), Vec::new());
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
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let path = e.path().map(|p| relative_to(&root, p)).unwrap_or_default();
                    self.skip(path, e.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_file() || kind(entry.path()).is_none() {
                continue;
            }
            let relative = relative_to(&root, entry.path());
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    self.skip(relative, format!("Cannot read metadata: {e}"));
                    continue;
                }
            };
            let size = meta.len() as i64;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i64);
            if let Some((old_size, old_mtime, chunks)) = known.remove(&relative) {
                if (old_size, old_mtime) == (size, mtime) {
                    let mut p = self.progress.lock().unwrap();
                    p.files += 1;
                    p.chunks += chunks;
                    continue;
                }
                stale.push(relative.clone());
            }
            pending.push((entry.into_path(), relative, size, mtime));
        }
        let removed = known.len();
        stale.extend(known.into_keys());
        if !stale.is_empty() {
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS stale(path TEXT PRIMARY KEY); DELETE FROM temp.stale;",
            )?;
            {
                let mut add = tx.prepare("INSERT OR IGNORE INTO temp.stale(path) VALUES(?1)")?;
                for path in &stale {
                    add.execute([path])?;
                }
            }
            tx.execute_batch(
                "DELETE FROM passages WHERE path IN (SELECT path FROM temp.stale);
                DELETE FROM files WHERE path IN (SELECT path FROM temp.stale);
                DELETE FROM temp.stale;",
            )?;
        }
        let mut updated = 0;
        {
            let mut insert =
                tx.prepare("INSERT INTO passages(path,line,section,text) VALUES(?1,?2,?3,?4)")?;
            let mut record = tx.prepare(
                "INSERT OR REPLACE INTO files(path,size,mtime,chunks) VALUES(?1,?2,?3,?4)",
            )?;
            for (path, relative, size, mtime) in pending {
                let text = match read_document(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.skip(relative, format!("{e:#}"));
                        continue;
                    }
                };
                let file = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
                let chunks = chunk_document(&text, kind(&path).unwrap_or(Kind::Plain), &file);
                for c in &chunks {
                    insert.execute(params![relative, c.line, c.section, c.text])?;
                }
                record.execute(params![relative, size, mtime, chunks.len()])?;
                updated += 1;
                let mut p = self.progress.lock().unwrap();
                p.files += 1;
                p.chunks += chunks.len();
                p.message = relative;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES('root',?1)",
            [root_text.as_str()],
        )?;
        tx.commit()?;
        Ok(if updated == 0 && removed == 0 {
            "Library ready · up to date".into()
        } else {
            format!("Library ready · {updated} updated, {removed} removed")
        })
    }
    /// BM25 search over passage text and the weighted section breadcrumb. Tries all
    /// terms (AND) first, then fills up with any-term (OR) matches; at most two
    /// passages per file. `Source.text` starts with "[File > H1 > H2]\n".
    pub fn search(&self, query: &str, pack: &str, limit: usize) -> Result<Vec<Source>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let conn = initialize(&self.db)?;
        let mut stmt = conn.prepare("SELECT path,line,section,text FROM passages WHERE passages MATCH ?1 AND (?2='' OR substr(path,1,length(?2)+1)=?2||'/') ORDER BY bm25(passages,0.0,0.0,4.0,1.0) LIMIT ?3")?;
        let fetch = (limit.clamp(1, 20) * 4) as i64;
        let mut run = |q: &str| -> Result<Vec<Source>> {
            Ok(stmt
                .query_map(params![q, pack, fetch], |r| {
                    let section: String = r.get(2)?;
                    let text: String = r.get(3)?;
                    Ok(Source {
                        path: r.get(0)?,
                        line: r.get(1)?,
                        text: if section.is_empty() {
                            text
                        } else {
                            format!("[{section}]\n{text}")
                        },
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let mut found = run(&terms.join(" AND "))?;
        if terms.len() > 1 && limit_per_file(found.clone(), PER_FILE, limit).len() < limit {
            let mut seen: HashSet<(String, usize)> =
                found.iter().map(|s| (s.path.clone(), s.line)).collect();
            found.extend(
                run(&terms.join(" OR "))?
                    .into_iter()
                    .filter(|s| seen.insert((s.path.clone(), s.line))),
            );
        }
        Ok(limit_per_file(found, PER_FILE, limit))
    }
}
/// Keeps ranking order, allowing at most `per_file` passages from one path.
pub fn limit_per_file(
    sources: impl IntoIterator<Item = Source>,
    per_file: usize,
    limit: usize,
) -> Vec<Source> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    sources
        .into_iter()
        .filter(|s| {
            let count = counts.entry(s.path.clone()).or_default();
            *count += 1;
            *count <= per_file
        })
        .take(limit)
        .collect()
}
fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Markdown,
    Rst,
    AsciiDoc,
    Plain,
}
fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}
// HTML is converted to Markdown-style headings by read_document.
fn kind(path: &Path) -> Option<Kind> {
    Some(match extension(path).as_str() {
        "md" | "mdx" | "html" | "htm" => Kind::Markdown,
        "rst" => Kind::Rst,
        "adoc" | "asc" => Kind::AsciiDoc,
        "txt" => Kind::Plain,
        _ => return None,
    })
}
pub fn read_document(path: &Path) -> Result<String> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= 2_000_000,
        "File exceeds 2 MB limit"
    );
    let raw = decode_text(&std::fs::read(path)?)?;
    if matches!(extension(path).as_str(), "html" | "htm") {
        let html = Html::parse_document(&raw);
        let node = ["main", "article", ".document", "body"]
            .iter()
            .find_map(|selector| html.select(&Selector::parse(selector).unwrap()).next());
        let mut out = String::new();
        if let Some(node) = node {
            html_text(node, &mut out);
        }
        Ok(out)
    } else {
        Ok(raw)
    }
}
// Visible text, one text node per line, with h1–h6 rewritten as "# Title" lines.
fn html_text(element: ElementRef, out: &mut String) {
    for child in element.children() {
        if let Some(e) = ElementRef::wrap(child) {
            let name = e.value().name();
            if matches!(name, "script" | "style" | "noscript" | "template") {
                continue;
            }
            let level = match name {
                "h1" => 1,
                "h2" => 2,
                "h3" => 3,
                "h4" => 4,
                "h5" => 5,
                "h6" => 6,
                _ => {
                    html_text(e, out);
                    continue;
                }
            };
            let title = e
                .text()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if !title.is_empty() {
                out.push_str(&format!("{} {title}\n", "#".repeat(level)));
            }
        } else if let Some(text) = child.value().as_text() {
            let text = text.trim();
            if !text.is_empty() {
                out.push_str(text);
                out.push('\n');
            }
        }
    }
}
/// Decodes UTF-8 (with or without BOM) and UTF-16 (BOM or detected), falling back to
/// Windows-1252 for other 8-bit text. Fails for binary data.
pub fn decode_text(bytes: &[u8]) -> Result<String> {
    if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        return Ok(utf16(rest, false));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        return Ok(utf16(rest, true));
    }
    let sample = &bytes[..bytes.len().min(4096)];
    let half = sample.len() / 2;
    if half >= 2 {
        let zeros = |skip: usize| {
            sample
                .iter()
                .skip(skip)
                .step_by(2)
                .filter(|b| **b == 0)
                .count()
        };
        let (even, odd) = (zeros(0), zeros(1));
        if odd * 10 >= half * 3 && even * 20 <= half {
            return Ok(utf16(bytes, false));
        }
        if even * 10 >= half * 3 && odd * 20 <= half {
            return Ok(utf16(bytes, true));
        }
    }
    anyhow::ensure!(!sample.contains(&0), "Binary file (contains NUL bytes)");
    Ok(match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => windows_1252(bytes),
    })
}
fn utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.as_chunks::<2>().0.iter().map(|c| {
        if big_endian {
            u16::from_be_bytes([c[0], c[1]])
        } else {
            u16::from_le_bytes([c[0], c[1]])
        }
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];
fn windows_1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252[(b - 0x80) as usize],
            _ => b as char,
        })
        .collect()
}
fn is_adornment(line: &str) -> bool {
    let mut chars = line.chars();
    chars.next().is_some_and(|c| {
        "=-`:'\"~^_*+#<>.".contains(c) && line.chars().count() >= 3 && chars.all(|d| d == c)
    })
}
// Per line: Some((level, title)) when that line starts a section.
fn headings(lines: &[&str], kind: Kind) -> Vec<Option<(usize, String)>> {
    let mut out = vec![None; lines.len()];
    match kind {
        Kind::Plain => {}
        Kind::Markdown => {
            let mut fence: Option<char> = None;
            let mut front_matter = lines.first().is_some_and(|l| l.trim() == "---");
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim_start();
                if front_matter {
                    front_matter = i == 0 || t.trim_end() != "---";
                    continue;
                }
                let marker = if t.starts_with("```") {
                    Some('`')
                } else if t.starts_with("~~~") {
                    Some('~')
                } else {
                    None
                };
                if let Some(m) = marker {
                    fence = match fence {
                        None => Some(m),
                        Some(f) if f == m => None,
                        other => other,
                    };
                    continue;
                }
                if fence.is_some() || line.len() - t.len() >= 4 {
                    continue;
                }
                let hashes = t.chars().take_while(|&c| c == '#').count();
                if (1..=6).contains(&hashes) {
                    let rest = &t[hashes..];
                    if rest.is_empty() || rest.starts_with([' ', '\t']) {
                        let title = rest.trim().trim_end_matches('#').trim();
                        if !title.is_empty() {
                            out[i] = Some((hashes, title.to_owned()));
                        }
                    }
                    continue;
                }
                let under = t.trim_end();
                let level = if !under.is_empty() && under.chars().all(|c| c == '=') {
                    1
                } else if under.len() >= 2 && under.chars().all(|c| c == '-') {
                    2
                } else {
                    continue;
                };
                if let Some(prev) = i.checked_sub(1).map(|p| lines[p].trim())
                    && !prev.is_empty()
                    && !prev.starts_with(['#', '`', '~', '-', '*', '>', '|'])
                    && out[i - 1].is_none()
                {
                    out[i - 1] = Some((level, prev.to_owned()));
                }
            }
        }
        Kind::Rst => {
            let mut styles: Vec<(char, bool)> = Vec::new();
            for i in 0..lines.len().saturating_sub(1) {
                let (title, under) = (lines[i].trim_end(), lines[i + 1].trim_end());
                if title.trim().is_empty()
                    || title.starts_with([' ', '\t'])
                    || is_adornment(title)
                    || !is_adornment(under)
                    || under.chars().count() < title.chars().count()
                {
                    continue;
                }
                let overline = i > 0 && lines[i - 1].trim_end() == under;
                let style = (under.chars().next().unwrap_or('='), overline);
                let level = match styles.iter().position(|s| *s == style) {
                    Some(p) => p + 1,
                    None => {
                        styles.push(style);
                        styles.len()
                    }
                };
                // An overline starts the section so it is not left in the previous chunk.
                out[if overline { i - 1 } else { i }] = Some((level, title.trim().to_owned()));
            }
        }
        Kind::AsciiDoc => {
            for (i, line) in lines.iter().enumerate() {
                let level = line.chars().take_while(|&c| c == '=').count();
                if (1..=6).contains(&level) && line[level..].starts_with(' ') {
                    let title = line[level..].trim();
                    if !title.is_empty() {
                        out[i] = Some((level, title.to_owned()));
                    }
                }
            }
        }
    }
    out
}
#[derive(Debug, PartialEq)]
struct Chunk {
    line: usize,
    section: String,
    text: String,
}
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>())
    }
}
// Splits at headings, then at CHUNK_CHARS within a section. Each chunk keeps its
// first line number and a "File > H1 > H2" breadcrumb.
fn chunk_document(text: &str, kind: Kind, file: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let heads = headings(&lines, kind);
    let (mut result, mut stack) = (Vec::new(), Vec::<(usize, String)>::new());
    let (mut buffer, mut start, mut section) = (String::new(), 1, file.to_owned());
    let flush = |buffer: &mut String, start: usize, section: &str, result: &mut Vec<Chunk>| {
        let text = std::mem::take(buffer);
        if !text.trim().is_empty() {
            result.push(Chunk {
                line: start,
                section: section.to_owned(),
                text,
            });
        }
    };
    for (i, line) in lines.iter().enumerate() {
        if let Some((level, title)) = &heads[i] {
            flush(&mut buffer, start, &section, &mut result);
            stack.retain(|(l, _)| l < level);
            stack.push((*level, clip(title, 80)));
            section = std::iter::once(file)
                .chain(stack.iter().map(|(_, t)| t.as_str()))
                .collect::<Vec<_>>()
                .join(" > ");
        }
        if line.is_empty() {
            if !buffer.is_empty() {
                buffer.push('\n');
            }
            continue;
        }
        for segment in line.chars().collect::<Vec<_>>().chunks(CHUNK_CHARS) {
            if buffer.chars().count() + segment.len() > CHUNK_CHARS {
                flush(&mut buffer, start, &section, &mut result);
            }
            if buffer.is_empty() {
                start = i + 1;
            }
            buffer.extend(segment.iter());
            buffer.push('\n');
        }
    }
    flush(&mut buffer, start, &section, &mut result);
    result
}
fn search_terms(query: &str) -> Vec<String> {
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
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_unicode() {
        let chunks = chunk_document(
            &format!("{}\nsecond", "é".repeat(1900)),
            Kind::Plain,
            "a.txt",
        );
        assert!(chunks.len() >= 2);
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.text.matches('é').count())
                .sum::<usize>(),
            1900
        );
        assert!(chunks.last().unwrap().text.contains("second"));
    }
    #[test]
    fn markdown_splits_at_headings_with_breadcrumbs() {
        let text = "---\ntitle: x\n---\nintro\n# Guide\nabout\n```\n# not a heading\n```\n## Install\nsteps\nUsage\n-----\nrun it\n# Other\nend";
        let chunks = chunk_document(text, Kind::Markdown, "guide.md");
        let sections: Vec<&str> = chunks.iter().map(|c| c.section.as_str()).collect();
        assert_eq!(
            sections,
            [
                "guide.md",
                "guide.md > Guide",
                "guide.md > Guide > Install",
                "guide.md > Guide > Usage",
                "guide.md > Other"
            ]
        );
        assert_eq!(chunks[2].line, 10);
        assert!(chunks[1].text.contains("# not a heading"));
    }
    #[test]
    fn rst_and_asciidoc_headings() {
        let rst = "=====\nTitle\n=====\nbody\nPart\n----\nmore\n====\nNext\n====\nlast";
        let sections: Vec<String> = chunk_document(rst, Kind::Rst, "a.rst")
            .into_iter()
            .map(|c| c.section)
            .collect();
        assert_eq!(sections.last().unwrap(), "a.rst > Next");
        assert!(sections.contains(&"a.rst > Title > Part".to_owned()));
        let adoc = chunk_document("= Doc\ntext\n== Sub\nmore", Kind::AsciiDoc, "a.adoc");
        assert_eq!(adoc[1].section, "a.adoc > Doc > Sub");
    }
    #[test]
    fn html_headings_become_sections() {
        let html = Html::parse_document(
            "<body><h1>API</h1><p>intro</p><script>x()</script><h2>Auth <b>tokens</b></h2><p>use keys</p></body>",
        );
        let mut out = String::new();
        html_text(
            html.select(&Selector::parse("body").unwrap())
                .next()
                .unwrap(),
            &mut out,
        );
        assert_eq!(out, "# API\nintro\n## Auth tokens\nuse keys\n");
    }
    #[test]
    fn decodes_bom_utf16_and_windows_1252() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi").unwrap(), "hi");
        assert_eq!(decode_text(b"\xFF\xFEh\0i\0").unwrap(), "hi");
        assert_eq!(decode_text(b"\xFE\xFF\0h\0i").unwrap(), "hi");
        assert_eq!(decode_text(b"h\0e\0l\0l\0o\0").unwrap(), "hello");
        assert_eq!(decode_text(b"caf\xE9 \x93q\x94").unwrap(), "café “q”");
        assert_eq!(decode_text("ok é".as_bytes()).unwrap(), "ok é");
        assert!(decode_text(b"\x7FELF\0\x01\x02\x03\0\0\0\0\x05").is_err());
    }
    #[test]
    fn search_prefers_all_terms_and_caps_files() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("localai-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let library = Library::new(dir.join("test.sqlite"));
        let c = initialize(&library.db)?;
        c.execute_batch(
            "DELETE FROM passages;
            INSERT INTO passages VALUES('rust/a.md',1,'a.md','ownership borrow reference');
            INSERT INTO passages VALUES('rust/a.md',30,'a.md > Borrow','ownership borrow');
            INSERT INTO passages VALUES('rust/a.md',60,'a.md','ownership');
            INSERT INTO passages VALUES('rust/c.md',1,'c.md','ownership checker');
            INSERT INTO passages VALUES('js/b.md',1,'b.md','ownership');",
        )?;
        let rust = library.search("ownership", "rust", 4)?;
        assert_eq!(rust.len(), 3);
        assert_eq!(rust.iter().filter(|s| s.path == "rust/a.md").count(), 2);
        assert_eq!(library.search("ownership", "js", 4)?[0].path, "js/b.md");
        let both = library.search("ownership checker", "", 1)?;
        assert_eq!(both[0].path, "rust/c.md");
        assert!(
            library.search("borrow", "rust", 1)?[0]
                .text
                .starts_with("[a.md > Borrow]\n")
        );
        drop(c);
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn incremental_index_skips_unchanged_and_removes_deleted() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("localai-index-{}", std::process::id()));
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs)?;
        std::fs::write(docs.join("a.md"), "# Alpha\nfirst")?;
        std::fs::write(docs.join("b.txt"), "second")?;
        std::fs::write(docs.join("bin.txt"), b"\0\0\0\0\x01")?;
        let library = Library::new(dir.join("index.sqlite"));
        assert!(library.index(&docs)?.contains("2 updated"));
        assert_eq!(library.progress.lock().unwrap().skipped_files.len(), 1);
        std::fs::remove_file(docs.join("b.txt"))?;
        assert!(library.index(&docs)?.contains("0 updated, 1 removed"));
        assert!(library.search("second", "", 4)?.is_empty());
        assert_eq!(library.search("first", "", 4)?.len(), 1);
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}

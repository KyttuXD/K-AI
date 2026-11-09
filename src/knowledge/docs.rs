use super::extract;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

// Bump when the passages/files layout changes: older databases are dropped once
// and rebuilt from scratch on the next index.
const SCHEMA: &str = "4";
const MAX_SKIPPED: usize = 200;
const CHUNK_CHARS: usize = 1800;
const BATCH_FILES: usize = 500;
const BATCH_TIME: Duration = Duration::from_secs(2);
pub const PER_FILE: usize = 2;
const IGNORED: [&str; 4] = [".git", "node_modules", "target", ".venv"];

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
/// Linked sources are indexed where they are; Imported ones are copies in `library_dir()`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SourceMode {
    Linked,
    Imported,
}
/// One folder or file of documentation. `name` is the pack name used to filter
/// searches and the first segment of every passage path ("name/relative/file.md").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocSource {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub mode: SourceMode,
    pub enabled: bool,
}
#[derive(Clone, Debug, Default)]
struct Indexed {
    name: String,
    root: String,
    files: usize,
    chunks: usize,
}
#[derive(Clone)]
pub struct Library {
    pub db: PathBuf,
    pub progress: Arc<Mutex<Progress>>,
    ready: Arc<AtomicBool>,
    reader: Arc<Mutex<Option<Connection>>>,
    // Sources with committed index rows, by id; refreshed after every run.
    indexed: Arc<Mutex<HashMap<String, Indexed>>>,
    // The configured sources as last passed to matches_sources (None: search everything).
    active: Arc<Mutex<Option<Vec<DocSource>>>>,
    queued: Arc<Mutex<Vec<DocSource>>>,
    // Removals still deleting rows; rebuilds wait in `queued` until they finish.
    removing: Arc<AtomicU64>,
}

pub fn library_dir() -> PathBuf {
    crate::config::data_dir().join("library")
}
pub fn supported(path: &Path) -> bool {
    kind(path).is_some()
}
fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:x}{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}
/// Lowercase ASCII letters and digits joined by single dashes, at most 40 characters.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(40).collect();
    match out.trim_end_matches('-') {
        "" => "docs".into(),
        s => s.into(),
    }
}
fn unique_dir(parent: &Path, slug: &str) -> PathBuf {
    let mut dir = parent.join(slug);
    for n in 2.. {
        if !dir.exists() {
            break;
        }
        dir = parent.join(format!("{slug}-{n}"));
    }
    dir
}
// Drops the Windows verbatim prefix that canonicalize adds, so paths read normally.
fn simplify(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}
fn default_name(path: &Path) -> String {
    let name = if path.is_file() {
        path.file_stem()
    } else {
        path.file_name()
    };
    name.map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Documentation".into())
}
fn ignored(name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| IGNORED.contains(&n))
}
/// A source indexed in place; `path` may be a folder or a single file.
pub fn link_path(path: &Path) -> DocSource {
    let path = simplify(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    DocSource {
        id: new_id(),
        name: default_name(&path),
        path,
        mode: SourceMode::Linked,
        enabled: true,
    }
}
/// Settings v4: the old single documentation folder becomes one Linked source.
pub fn migrate_docs_dir(old: &Path) -> Vec<DocSource> {
    if old.is_dir() {
        vec![link_path(old)]
    } else {
        vec![]
    }
}
fn note_skip(progress: &Mutex<Progress>, path: String, reason: String) {
    let mut p = progress.lock().unwrap();
    p.skipped += 1;
    if p.skipped_files.len() < MAX_SKIPPED {
        p.skipped_files.push((path, reason));
    }
}
/// Copies the supported files of `paths` into a new folder under `library_dir()`. One
/// folder is copied as its contents; otherwise each folder or file keeps its own name.
/// Updates `files`, `message` and the skip fields of `progress` (never `running`, so pass
/// a Progress of your own, not the Library's).
pub fn import_paths(
    paths: &[PathBuf],
    name: &str,
    progress: Arc<Mutex<Progress>>,
) -> Result<DocSource> {
    import_into(&library_dir(), paths, name, &progress)
}
fn import_into(
    library: &Path,
    paths: &[PathBuf],
    name: &str,
    progress: &Mutex<Progress>,
) -> Result<DocSource> {
    anyhow::ensure!(!paths.is_empty(), "Nothing to import");
    let name = match name.trim() {
        "" => default_name(&paths[0]),
        n => n.to_owned(),
    };
    let dest = unique_dir(library, &slug(&name));
    std::fs::create_dir_all(&dest).context("Cannot create the library folder")?;
    let single_dir = paths.len() == 1 && paths[0].is_dir();
    let mut copied = 0;
    for path in paths {
        let base = if single_dir {
            path.as_path()
        } else {
            path.parent().unwrap_or(path)
        };
        let walk = WalkDir::new(path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !ignored(e.file_name()) && !e.path().starts_with(&dest));
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let at = e.path().unwrap_or(path.as_path()).display().to_string();
                    note_skip(progress, at, e.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_file() || !supported(entry.path()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(base)
                .unwrap_or(Path::new(entry.file_name()));
            let target = dest.join(relative);
            let result = target
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::fs::copy(entry.path(), &target));
            let shown = relative.to_string_lossy().replace('\\', "/");
            match result {
                Ok(_) => {
                    copied += 1;
                    let mut p = progress.lock().unwrap();
                    p.files += 1;
                    p.message = shown;
                }
                Err(e) => note_skip(progress, shown, format!("Cannot copy: {e}")),
            }
        }
    }
    if copied == 0 {
        let _ = std::fs::remove_dir_all(&dest);
        anyhow::bail!("No supported documents found");
    }
    Ok(DocSource {
        id: new_id(),
        name,
        path: dest,
        mode: SourceMode::Imported,
        enabled: true,
    })
}
/// Deletes an Imported source's copy. Refuses anything outside `library_dir()`.
pub fn delete_imported(source: &DocSource) -> Result<()> {
    delete_within(&library_dir(), source)
}
fn delete_within(library: &Path, source: &DocSource) -> Result<()> {
    anyhow::ensure!(
        source.mode == SourceMode::Imported,
        "Only imported copies can be deleted"
    );
    let library = library.canonicalize().context("Library folder not found")?;
    let path = source
        .path
        .canonicalize()
        .context("Imported copy not found")?;
    anyhow::ensure!(
        path.starts_with(&library) && path != library,
        "Refusing to delete outside the library folder"
    );
    if path.is_dir() {
        std::fs::remove_dir_all(&path)?;
    } else {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}
fn root_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn open(db: &Path) -> Result<Connection> {
    let conn = Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(10))?;
    // NORMAL is crash-safe in WAL mode; it can only lose the last commit on power loss.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        PRAGMA journal_size_limit=67108864; PRAGMA temp_store=MEMORY;",
    )?;
    Ok(conn)
}
fn setup(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT);")?;
    let version: Option<String> = conn
        .query_row("SELECT value FROM metadata WHERE key='schema'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if version.as_deref() != Some(SCHEMA) {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
            DROP TABLE IF EXISTS passages; DROP TABLE IF EXISTS passage_files;
            DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS sources;
            DELETE FROM metadata;
            INSERT INTO metadata(key,value) VALUES('schema','{SCHEMA}');
            COMMIT;"
        ))?;
    }
    conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS passages USING fts5(source UNINDEXED, path UNINDEXED, line UNINDEXED, section, text, tokenize='porter unicode61');
        CREATE TABLE IF NOT EXISTS files(source TEXT, path TEXT, size INTEGER, mtime INTEGER, chunks INTEGER, PRIMARY KEY(source, path));
        CREATE TABLE IF NOT EXISTS sources(id TEXT PRIMARY KEY, name TEXT, root TEXT);
        CREATE TABLE IF NOT EXISTS passage_files(id INTEGER PRIMARY KEY, source TEXT, path TEXT);
        CREATE INDEX IF NOT EXISTS passage_files_by_path ON passage_files(source, path);")?;
    Ok(())
}
/// Deletes a source's passages through the indexed side table: filtering the FTS table on
/// its UNINDEXED columns would scan every passage.
fn drop_passages(conn: &Connection, source: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM passages WHERE rowid IN (SELECT id FROM passage_files WHERE source=?1)",
        [source],
    )?;
    conn.execute("DELETE FROM passage_files WHERE source=?1", [source])?;
    Ok(())
}
#[cfg(test)]
fn initialize(db: &Path) -> Result<Connection> {
    let conn = open(db)?;
    setup(&conn)?;
    Ok(conn)
}
type Parsed = (String, i64, i64, Result<Vec<Chunk>>);

impl Library {
    pub fn new(db: PathBuf) -> Self {
        let library = Self {
            db,
            progress: Default::default(),
            ready: Default::default(),
            reader: Default::default(),
            indexed: Default::default(),
            active: Default::default(),
            queued: Default::default(),
            removing: Default::default(),
        };
        let state = match library.connect().and_then(|c| library.load_stats(&c)) {
            Ok(()) => {
                let (files, chunks) = library.totals(|_| true);
                Progress {
                    files,
                    chunks,
                    message: if chunks > 0 { "Library ready" } else { "" }.into(),
                    ..Default::default()
                }
            }
            Err(e) => Progress {
                message: format!("Index error: {e}"),
                ..Default::default()
            },
        };
        *library.progress.lock().unwrap() = state;
        library
    }
    // Opens a connection; the schema is checked once per Library.
    fn connect(&self) -> Result<Connection> {
        let conn = open(&self.db)?;
        if !self.ready.load(Ordering::Acquire) {
            setup(&conn)?;
            self.ready.store(true, Ordering::Release);
        }
        Ok(conn)
    }
    // Per-source totals from the small files table (never the passage text).
    fn load_stats(&self, conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.name, s.root, count(f.path), coalesce(sum(f.chunks),0)
            FROM sources s LEFT JOIN files f ON f.source=s.id GROUP BY s.id",
        )?;
        let stats = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Indexed {
                        name: r.get(1)?,
                        root: r.get(2)?,
                        files: r.get(3)?,
                        chunks: r.get(4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        *self.indexed.lock().unwrap() = stats;
        Ok(())
    }
    fn totals(&self, include: impl Fn(&str) -> bool) -> (usize, usize) {
        self.indexed
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| include(id))
            .fold((0, 0), |(f, c), (_, i)| (f + i.files, c + i.chunks))
    }
    /// (files, passages) last indexed for a source; no database access.
    #[cfg(test)]
    pub fn source_stats(&self, id: &str) -> Option<(usize, usize)> {
        self.indexed
            .lock()
            .unwrap()
            .get(id)
            .map(|i| (i.files, i.chunks))
    }
    /// True when every enabled source has an index under its current name and path (a
    /// first index counts as soon as it starts). Also records `sources` as the set that
    /// `search` filters by, so call it with the configured list before searching.
    pub fn matches_sources(&self, sources: &[DocSource]) -> bool {
        *self.active.lock().unwrap() = Some(sources.to_vec());
        let indexed = self.indexed.lock().unwrap();
        let mut enabled = sources.iter().filter(|s| s.enabled).peekable();
        enabled.peek().is_some()
            && enabled.all(|s| {
                indexed
                    .get(&s.id)
                    .is_some_and(|i| i.name == s.name && i.root == root_text(&s.path))
            })
    }
    /// Incrementally indexes the enabled `sources` on a background thread. While a run is
    /// in progress the request is queued and runs right after it.
    pub fn rebuild_sources(&self, sources: Vec<DocSource>) {
        let sources: Vec<DocSource> = sources.into_iter().filter(|s| s.enabled).collect();
        if sources.is_empty() {
            return;
        }
        {
            let mut p = self.progress.lock().unwrap();
            if p.running || self.removing.load(Ordering::SeqCst) > 0 {
                let mut queued = self.queued.lock().unwrap();
                for s in sources {
                    queued.retain(|q| q.id != s.id);
                    queued.push(s);
                }
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
            let mut batch = sources;
            loop {
                let result = library.index(&batch);
                let mut p = library.progress.lock().unwrap();
                p.message = match result {
                    Ok(message) => message,
                    Err(e) => format!("Index failed: {e:#}"),
                };
                // A removal still running starts the queued sources itself when it is done.
                batch = if library.removing.load(Ordering::SeqCst) == 0 {
                    std::mem::take(&mut *library.queued.lock().unwrap())
                } else {
                    Vec::new()
                };
                if batch.is_empty() {
                    p.running = false;
                    break;
                }
            }
        });
    }
    /// Drops a source's passages in the background; rebuilds requested meanwhile run after it.
    /// Deleting an Imported copy is separate (`delete_imported`).
    pub fn remove_source(&self, id: &str) {
        self.indexed.lock().unwrap().remove(id);
        self.queued.lock().unwrap().retain(|s| s.id != id);
        if let Some(active) = self.active.lock().unwrap().as_mut() {
            active.retain(|s| s.id != id);
        }
        let (library, id) = (self.clone(), id.to_owned());
        self.removing.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let result = library.delete_source(&id);
            let queued = {
                let mut p = library.progress.lock().unwrap();
                library.removing.fetch_sub(1, Ordering::SeqCst);
                match result {
                    Err(e) => p.message = format!("Remove failed: {e:#}"),
                    Ok(()) if !p.running => (p.files, p.chunks) = library.totals(|_| true),
                    Ok(()) => {}
                }
                if p.running || library.removing.load(Ordering::SeqCst) > 0 {
                    Vec::new()
                } else {
                    std::mem::take(&mut *library.queued.lock().unwrap())
                }
            };
            if !queued.is_empty() {
                library.rebuild_sources(queued);
            }
        });
    }
    fn delete_source(&self, id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        drop_passages(&tx, id)?;
        tx.execute("DELETE FROM files WHERE source=?1", [id])?;
        tx.execute("DELETE FROM sources WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }
    fn skip(&self, path: String, reason: String) {
        note_skip(&self.progress, path, reason);
    }
    fn index(&self, sources: &[DocSource]) -> Result<String> {
        let ids: HashSet<&str> = sources.iter().map(|s| s.id.as_str()).collect();
        let (files, chunks) = self.totals(|id| !ids.contains(id));
        {
            let mut p = self.progress.lock().unwrap();
            (p.files, p.chunks) = (files, chunks);
        }
        let conn = self.connect()?;
        conn.execute_batch("PRAGMA cache_size=-16000;")?;
        let (mut updated, mut removed, mut failed) = (0, 0, Vec::new());
        let enabled: Vec<&DocSource> = sources.iter().filter(|s| s.enabled).collect();
        for source in &enabled {
            match self.index_source(&conn, source) {
                Ok((u, r)) => (updated, removed) = (updated + u, removed + r),
                Err(e) => {
                    if !conn.is_autocommit() {
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                    failed.push(format!("{}: {e:#}", source.name));
                }
            }
        }
        // Fewer FTS5 segments after a large run keep queries fast.
        if updated > 1000 {
            self.progress.lock().unwrap().message = "Optimizing index…".into();
            conn.execute("INSERT INTO passages(passages) VALUES('optimize')", [])?;
        } else if updated + removed > 0 {
            conn.execute(
                "INSERT INTO passages(passages, rank) VALUES('merge', 500)",
                [],
            )?;
        }
        self.load_stats(&conn)?;
        anyhow::ensure!(
            enabled.is_empty() || failed.len() < enabled.len(),
            "{}",
            failed.join("; ")
        );
        let mut message = if updated == 0 && removed == 0 {
            "Library ready · up to date".to_owned()
        } else {
            format!("Library ready · {updated} updated, {removed} removed")
        };
        if !failed.is_empty() {
            message.push_str(&format!(" · {}", failed.join("; ")));
        }
        Ok(message)
    }
    // Incremental: files whose path, size and mtime match the last run keep their
    // passages; changed and deleted files are removed before re-reading. The source row
    // and deletions commit first, then new passages commit every 500 files or 2 s, so
    // search works during a first index and an interrupted run resumes where it stopped.
    fn index_source(&self, conn: &Connection, src: &DocSource) -> Result<(usize, usize)> {
        let root = src
            .path
            .canonicalize()
            .with_context(|| format!("{} not found", src.path.display()))?;
        let base = if root.is_file() {
            root.parent().map(Path::to_path_buf).unwrap_or_default()
        } else {
            root.clone()
        };
        let root_saved = root_text(&src.path);
        let prefix = format!("{}/", src.name);
        let saved: Option<(String, String)> = conn
            .query_row(
                "SELECT name, root FROM sources WHERE id=?1",
                [&src.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let reset = saved != Some((src.name.clone(), root_saved.clone()));
        let mut known: HashMap<String, (i64, i64, usize)> = HashMap::new();
        if !reset {
            let mut stmt =
                conn.prepare("SELECT path,size,mtime,chunks FROM files WHERE source=?1")?;
            let rows = stmt.query_map([&src.id], |r| {
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
        let walk = WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !ignored(e.file_name()));
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    let path = e.path().map(|p| relative_to(&base, p)).unwrap_or_default();
                    self.skip(format!("{prefix}{path}"), e.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_file() || !supported(entry.path()) {
                continue;
            }
            let relative = relative_to(&base, entry.path());
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(e) => {
                    self.skip(
                        format!("{prefix}{relative}"),
                        format!("Cannot read metadata: {e}"),
                    );
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
        conn.execute_batch("BEGIN IMMEDIATE")?;
        if reset {
            drop_passages(conn, &src.id)?;
            conn.execute("DELETE FROM files WHERE source=?1", [&src.id])?;
        }
        if !stale.is_empty() {
            conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS stale(path TEXT PRIMARY KEY); DELETE FROM temp.stale;",
            )?;
            {
                let mut add = conn.prepare("INSERT OR IGNORE INTO temp.stale(path) VALUES(?1)")?;
                for path in &stale {
                    add.execute([path])?;
                }
            }
            conn.execute(
                "DELETE FROM passages WHERE rowid IN (SELECT id FROM passage_files
                    WHERE source=?1 AND path IN (SELECT ?2||path FROM temp.stale))",
                params![src.id, prefix],
            )?;
            conn.execute(
                "DELETE FROM passage_files WHERE source=?1 AND path IN (SELECT ?2||path FROM temp.stale)",
                params![src.id, prefix],
            )?;
            conn.execute(
                "DELETE FROM files WHERE source=?1 AND path IN (SELECT path FROM temp.stale)",
                [&src.id],
            )?;
            conn.execute_batch("DELETE FROM temp.stale;")?;
        }
        conn.execute(
            "INSERT OR REPLACE INTO sources(id,name,root) VALUES(?1,?2,?3)",
            params![src.id, src.name, root_saved],
        )?;
        conn.execute_batch("COMMIT")?;
        {
            let mut indexed = self.indexed.lock().unwrap();
            let entry = indexed.entry(src.id.clone()).or_default();
            if reset {
                *entry = Indexed::default();
            }
            (entry.name, entry.root) = (src.name.clone(), root_saved);
        }
        if pending.is_empty() {
            return Ok((0, removed));
        }
        // Reading, extraction and chunking run on worker threads; this thread inserts.
        let workers = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .saturating_sub(1)
            .clamp(1, pending.len());
        let queue = Mutex::new(pending.into_iter());
        let (send, recv) = mpsc::sync_channel::<Parsed>(32);
        let mut updated = 0;
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let (send, queue) = (send.clone(), &queue);
                scope.spawn(move || {
                    loop {
                        let next = queue.lock().unwrap().next();
                        let Some((path, relative, size, mtime)) = next else {
                            break;
                        };
                        let file = relative.rsplit('/').next().unwrap_or(&relative).to_owned();
                        let chunks = extract::extract_text(&path).map(|text| {
                            chunk_document(&text, kind(&path).unwrap_or(Kind::Plain), &file)
                        });
                        if send.send((relative, size, mtime, chunks)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(send);
            let (mut in_batch, mut started) = (0, Instant::now());
            conn.execute_batch("BEGIN IMMEDIATE")?;
            for (relative, size, mtime, chunks) in recv {
                let shown = format!("{prefix}{relative}");
                let chunks = match chunks {
                    Ok(chunks) => chunks,
                    Err(e) => {
                        self.skip(shown, format!("{e:#}"));
                        continue;
                    }
                };
                {
                    let mut insert = conn.prepare_cached(
                        "INSERT INTO passages(source,path,line,section,text) VALUES(?1,?2,?3,?4,?5)",
                    )?;
                    let mut link = conn.prepare_cached(
                        "INSERT INTO passage_files(id,source,path) VALUES(last_insert_rowid(),?1,?2)",
                    )?;
                    for c in &chunks {
                        insert.execute(params![src.id, shown, c.line, c.section, c.text])?;
                        link.execute(params![src.id, shown])?;
                    }
                }
                conn.prepare_cached(
                    "INSERT OR REPLACE INTO files(source,path,size,mtime,chunks) VALUES(?1,?2,?3,?4,?5)",
                )?
                .execute(params![src.id, relative, size, mtime, chunks.len()])?;
                updated += 1;
                in_batch += 1;
                {
                    let mut p = self.progress.lock().unwrap();
                    p.files += 1;
                    p.chunks += chunks.len();
                    p.message = shown;
                }
                if in_batch >= BATCH_FILES || started.elapsed() >= BATCH_TIME {
                    conn.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
                    (in_batch, started) = (0, Instant::now());
                }
            }
            conn.execute_batch("COMMIT")?;
            Ok(())
        })?;
        Ok((updated, removed))
    }
    /// BM25 search over passage text and the weighted section breadcrumb. Tries all
    /// terms (AND) first, then fills up with any-term (OR) matches; at most two
    /// passages per file. `pack` is a source name ("" = every enabled source).
    /// `Source.text` starts with "[File > H1 > H2]\n".
    pub fn search(&self, query: &str, pack: &str, limit: usize) -> Result<Vec<Source>> {
        let terms = search_terms(query);
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let wanted = |name: &str| pack.is_empty() || name == pack;
        let ids: Vec<String> = match &*self.active.lock().unwrap() {
            Some(sources) => sources
                .iter()
                .filter(|s| s.enabled && wanted(&s.name))
                .map(|s| s.id.clone())
                .collect(),
            None => self
                .indexed
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, i)| wanted(&i.name))
                .map(|(id, _)| id.clone())
                .collect(),
        };
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let ids = format!("\u{1f}{}\u{1f}", ids.join("\u{1f}"));
        let mut reader = self.reader.lock().unwrap();
        let conn = match reader.take() {
            Some(conn) => conn,
            None => self.connect()?,
        };
        let conn = reader.insert(conn);
        let mut stmt = conn.prepare_cached("SELECT path,line,section,text FROM passages WHERE passages MATCH ?1 AND instr(?2, char(31)||source||char(31))>0 ORDER BY bm25(passages,0.0,0.0,0.0,4.0,1.0) LIMIT ?3")?;
        let fetch = (limit.clamp(1, 20) * 4) as i64;
        let mut run = |q: &str| -> Result<Vec<Source>> {
            Ok(stmt
                .query_map(params![q, ids, fetch], |r| {
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
// HTML (read_document) and DOCX (extract) are converted to Markdown-style headings.
fn kind(path: &Path) -> Option<Kind> {
    Some(match extension(path).as_str() {
        "md" | "mdx" | "markdown" | "html" | "htm" | "docx" => Kind::Markdown,
        "rst" => Kind::Rst,
        "adoc" | "asciidoc" => Kind::AsciiDoc,
        "txt" | "pdf" => Kind::Plain,
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
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("localai-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn source(id: &str, name: &str, path: &Path) -> DocSource {
        DocSource {
            id: id.into(),
            name: name.into(),
            path: path.into(),
            mode: SourceMode::Linked,
            enabled: true,
        }
    }
    #[test]
    fn search_prefers_all_terms_and_caps_files() -> Result<()> {
        let dir = temp("search");
        let library = Library::new(dir.join("test.sqlite"));
        let c = initialize(&library.db)?;
        c.execute_batch(
            "DELETE FROM passages;
            INSERT INTO passages VALUES('r','rust/a.md',1,'a.md','ownership borrow reference');
            INSERT INTO passages VALUES('r','rust/a.md',30,'a.md > Borrow','ownership borrow');
            INSERT INTO passages VALUES('r','rust/a.md',60,'a.md','ownership');
            INSERT INTO passages VALUES('r','rust/c.md',1,'c.md','ownership checker');
            INSERT INTO passages VALUES('j','js/b.md',1,'b.md','ownership');",
        )?;
        let mut sources = vec![source("r", "rust", &dir), source("j", "js", &dir)];
        assert!(!library.matches_sources(&sources));
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
        sources[1].enabled = false;
        library.matches_sources(&sources);
        assert!(library.search("ownership", "js", 4)?.is_empty());
        assert!(
            library
                .search("ownership", "", 8)?
                .iter()
                .all(|s| s.path.starts_with("rust/"))
        );
        drop(c);
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn incremental_index_skips_unchanged_and_removes_deleted() -> Result<()> {
        let dir = temp("index");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs)?;
        std::fs::write(docs.join("a.md"), "# Alpha\nfirst")?;
        std::fs::write(docs.join("b.txt"), "second")?;
        std::fs::write(docs.join("bin.txt"), b"\0\0\0\0\x01")?;
        std::fs::write(docs.join("image.png"), "not indexed")?;
        let linked = source("d", "docs", &docs);
        let library = Library::new(dir.join("index.sqlite"));
        assert!(
            library
                .index(std::slice::from_ref(&linked))?
                .contains("2 updated")
        );
        assert_eq!(library.progress.lock().unwrap().skipped_files.len(), 1);
        assert!(library.matches_sources(std::slice::from_ref(&linked)));
        assert_eq!(library.search("first", "docs", 4)?[0].path, "docs/a.md");
        std::fs::remove_file(docs.join("b.txt"))?;
        assert!(
            library
                .index(std::slice::from_ref(&linked))?
                .contains("0 updated, 1 removed")
        );
        assert!(library.search("second", "", 4)?.is_empty());
        assert_eq!(library.search("first", "", 4)?.len(), 1);
        // A renamed source is reindexed under its new pack name.
        let renamed = DocSource {
            name: "guide".into(),
            ..linked.clone()
        };
        assert!(!library.matches_sources(std::slice::from_ref(&renamed)));
        assert!(
            library
                .index(std::slice::from_ref(&renamed))?
                .contains("1 updated")
        );
        assert_eq!(library.search("first", "guide", 4)?[0].path, "guide/a.md");
        library.delete_source("d")?;
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn new_reports_totals_from_files_table() -> Result<()> {
        let dir = temp("totals");
        let docs = dir.join("docs");
        std::fs::create_dir_all(&docs)?;
        std::fs::write(docs.join("a.md"), "# One\nalpha\n# Two\nbeta")?;
        std::fs::write(docs.join("b.txt"), "gamma")?;
        let single = dir.join("single.md");
        std::fs::write(&single, "delta")?;
        let db = dir.join("index.sqlite");
        let first = Library::new(db.clone());
        let sources = [source("a", "docs", &docs), source("b", "single", &single)];
        first.index(&sources)?;
        drop(first);
        let library = Library::new(db);
        let p = library.progress.lock().unwrap().clone();
        assert_eq!((p.files, p.chunks), (3, 4));
        assert_eq!(library.source_stats("a"), Some((2, 3)));
        assert!(library.matches_sources(&sources));
        assert_eq!(
            library.search("delta", "single", 4)?[0].path,
            "single/single.md"
        );
        library.delete_source("a")?;
        library.load_stats(&library.connect()?)?;
        assert_eq!(library.source_stats("a"), None);
        assert!(library.search("alpha", "", 4)?.is_empty());
        drop(library);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn supported_extensions() {
        for ok in [
            "a.md",
            "a.MDX",
            "a.markdown",
            "a.txt",
            "a.rst",
            "a.adoc",
            "a.asciidoc",
            "a.html",
            "a.htm",
            "a.pdf",
            "a.docx",
        ] {
            assert!(supported(Path::new(ok)), "{ok}");
        }
        for no in ["a.png", "a.doc", "a", "a.asc", "a.rs"] {
            assert!(!supported(Path::new(no)), "{no}");
        }
    }
    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(slug("Rust Book (2024)!"), "rust-book-2024");
        assert_eq!(slug("  ..//  "), "docs");
        assert_eq!(slug("Ünïcode Doks"), "n-code-doks");
        assert_eq!(slug(&"x".repeat(60)).len(), 40);
        let dir = temp("unique");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        assert_eq!(unique_dir(&dir, "docs"), dir.join("docs-2"));
        assert_eq!(unique_dir(&dir, "other"), dir.join("other"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn import_copies_supported_files_and_delete_stays_inside() -> Result<()> {
        let dir = temp("import");
        let (src, lib) = (dir.join("src"), dir.join("library"));
        std::fs::create_dir_all(src.join("sub/.git"))?;
        std::fs::write(src.join("a.md"), "a")?;
        std::fs::write(src.join("sub/b.txt"), "b")?;
        std::fs::write(src.join("sub/.git/c.md"), "c")?;
        std::fs::write(src.join("logo.png"), "x")?;
        let progress = Mutex::new(Progress::default());
        let imported = import_into(&lib, std::slice::from_ref(&src), "My Docs", &progress)?;
        assert_eq!(imported.mode, SourceMode::Imported);
        assert_eq!(imported.path, lib.join("my-docs"));
        assert!(imported.path.join("sub/b.txt").is_file());
        assert!(!imported.path.join("sub/.git").exists());
        assert_eq!(progress.lock().unwrap().files, 2);
        // Several inputs keep their own names.
        let two = import_into(&lib, &[src.join("a.md"), src.join("sub")], "", &progress)?;
        assert_eq!(two.name, "a");
        assert!(two.path.join("a.md").is_file() && two.path.join("sub/b.txt").is_file());
        assert!(import_into(&lib, &[src.join("logo.png")], "x", &progress).is_err());
        let outside = DocSource {
            mode: SourceMode::Imported,
            ..source("o", "o", &src)
        };
        assert!(delete_within(&lib, &outside).is_err());
        assert!(
            delete_within(
                &lib,
                &DocSource {
                    mode: SourceMode::Linked,
                    ..imported.clone()
                }
            )
            .is_err()
        );
        delete_within(&lib, &imported)?;
        assert!(!imported.path.exists() && src.join("a.md").exists());
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn link_and_migrate_old_docs_folder() {
        let dir = temp("migrate");
        let docs = dir.join("Engine Docs");
        std::fs::create_dir_all(&docs).unwrap();
        let migrated = migrate_docs_dir(&docs);
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].name, "Engine Docs");
        assert_eq!(migrated[0].mode, SourceMode::Linked);
        assert!(migrated[0].enabled && migrated[0].path.is_dir());
        assert!(!migrated[0].path.to_string_lossy().starts_with(r"\\?\"));
        assert!(migrate_docs_dir(&dir.join("missing")).is_empty());
        std::fs::write(dir.join("notes.md"), "n").unwrap();
        assert_eq!(link_path(&dir.join("notes.md")).name, "notes");
        assert_ne!(link_path(&docs).id, link_path(&docs).id);
        std::fs::remove_dir_all(dir).unwrap();
    }
    /// Run by .github/workflows/perf.yml; prints one `PERF <name> <value> <unit>` line.
    #[test]
    #[ignore = "perf"]
    fn perf_docs_chunk_1mb() {
        let section = "## Section\nSome words about ownership, borrowing and lifetimes.\n";
        let text = section.repeat((1 << 20) / section.len());
        let start = Instant::now();
        let chunks = chunk_document(&text, Kind::Markdown, "big.md");
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(!chunks.is_empty());
        // The leading newline keeps the line apart from libtest's "test … ..." prefix.
        println!("\nPERF docs_chunk_1mb {ms:.1} ms");
    }
}

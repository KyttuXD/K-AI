//! Workbench page: git review and checkpoints, a small code editor and task terminals,
//! plus the Ctrl+P quick-open palette.
use super::*;
use crate::core::{git, terminal};
use eframe::egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
    sync::mpsc::TryRecvError,
};

/// Sidebar index of the Workbench page; mod.rs routes this value to `workbench_page`.
pub(super) const WORKBENCH_VIEW: usize = 6;
/// Largest file the editor opens (tools::write accepts up to 128 KB).
const EDIT_LIMIT: u64 = 128_000;
/// Largest file whose bytes are held in memory so Undo can restore a discarded file.
const UNDO_LIMIT: u64 = 8_000_000;
/// Bytes of a diff or of terminal output drawn at once.
const VIEW_LIMIT: usize = 200_000;
const MAX_MATCHES: usize = 10_000;
const PALETTE_ROWS: usize = 40;
const LIST_ROWS: usize = 5_000;
const REFRESH: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Default, PartialEq)]
enum WbTab {
    #[default]
    Git,
    Editor,
    Terminal,
}

struct GitSnapshot {
    repo: bool,
    files: Vec<git::GitFile>,
    checkpoints: Vec<(String, String)>,
}
enum GitMsg {
    Status(Result<GitSnapshot, String>),
    /// Path, staged side, diff text.
    Diff(String, bool, Result<String, String>),
    /// Result message, and whether files in the worktree may have changed.
    Done(Result<String, String>, bool),
    /// A checkpoint restore: message and the id of the safety checkpoint that undoes it.
    Restored(Result<(String, Option<String>), String>),
}
enum GitAct {
    Refresh,
    Discard(String),
    Restore(String, String),
    Select(String, bool),
    Stage(Vec<String>),
    Unstage(Vec<String>),
    Open(String),
    Checkpoint,
}
/// Find-match byte ranges (start, end) in the editor text.
type Spans = Arc<Vec<(usize, usize)>>;

#[derive(Default)]
struct EditorState {
    /// Workspace the file was opened from, so a later workspace switch cannot redirect a save.
    root: PathBuf,
    path: String,
    text: String,
    /// The file as last read or written; the expected content for the stale check.
    saved: String,
    find_open: bool,
    find: String,
    focus_find: bool,
    current: usize,
    jump: bool,
    show_diff: bool,
    /// Bumped on every edit; keys the caches below so no frame redoes whole-file work.
    rev: u64,
    matches: Option<(u64, String, Spans)>,
    diff: Option<(u64, String)>,
    diff_job: Option<(bool, LayoutJob)>,
    /// Laid-out text, reused while text, marks, theme and wrap width are unchanged.
    galley: Option<(u64, Arc<egui::Galley>)>,
}
impl EditorState {
    fn dirty(&self) -> bool {
        self.text != self.saved
    }
}

#[derive(Default)]
struct QuickOpen {
    query: String,
    results: Vec<String>,
    last: Option<String>,
    selected: usize,
}

#[derive(Default)]
pub struct WorkbenchState {
    tab: WbTab,
    root: PathBuf,
    repo: Option<bool>,
    git_error: Option<String>,
    changes: Vec<git::GitFile>,
    checkpoints: Vec<(String, String)>,
    selected: Option<(String, bool)>,
    diff: String,
    diff_job: Option<(bool, LayoutJob)>,
    label: String,
    pending: Vec<Receiver<GitMsg>>,
    last_refresh: Option<Instant>,
    editor: EditorState,
    files: Option<Vec<String>>,
    files_rx: Option<Receiver<Vec<String>>>,
    filter: String,
    hits: Option<(String, Vec<String>)>,
    quick: Option<QuickOpen>,
    profile: Option<usize>,
    args_text: String,
    sessions: Vec<terminal::Session>,
    active: usize,
    /// Terminal output layout keyed by session, output generation, colour and width.
    term_galley: Option<(u64, Arc<egui::Galley>)>,
}

impl WorkbenchState {
    /// Stops every task and what it started; call when the app closes.
    pub fn shutdown(&mut self) {
        for s in &self.sessions {
            s.stop();
        }
    }
    fn busy(&self) -> bool {
        !self.pending.is_empty()
    }
    fn spawn(&mut self, job: impl FnOnce() -> GitMsg + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        self.pending.push(rx);
    }
    fn refresh(&mut self) {
        let root = self.root.clone();
        self.last_refresh = Some(Instant::now());
        self.git_error = None;
        self.spawn(move || GitMsg::Status(snapshot(&root).map_err(|e| e.to_string())));
    }
    fn load_diff(&mut self) {
        let Some((path, staged)) = self.selected.clone() else {
            return;
        };
        let untracked = !staged
            && self
                .changes
                .iter()
                .any(|f| f.path == path && f.worktree == '?');
        let root = self.root.clone();
        self.spawn(move || {
            let diff = if untracked {
                untracked_diff(&root, &path)
            } else {
                git::diff(&root, Some(&path), staged)
            };
            GitMsg::Diff(path, staged, diff.map_err(|e| e.to_string()))
        });
    }
    fn git_op(
        &mut self,
        files_changed: bool,
        op: impl FnOnce(&Path) -> anyhow::Result<String> + Send + 'static,
    ) {
        let root = self.root.clone();
        self.spawn(move || GitMsg::Done(op(&root).map_err(|e| e.to_string()), files_changed));
    }
    /// Lists workspace files in the background (ignore-aware); the old list stays meanwhile.
    fn load_files(&mut self) {
        if self.files_rx.is_none() {
            let root = self.root.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(tools::list_ignored_aware(&root));
            });
            self.files_rx = Some(rx);
        }
    }
    fn poll_files(&mut self) {
        let Some(rx) = &self.files_rx else { return };
        let files = match rx.try_recv() {
            Ok(files) => files,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => vec![],
        };
        self.files = Some(files);
        self.files_rx = None;
        self.hits = None;
        if let Some(q) = &mut self.quick {
            q.last = None;
        }
    }
    fn any_running(&self) -> bool {
        self.sessions
            .iter()
            .any(|s| s.running.load(Ordering::Relaxed))
    }
}

/// Restores a checkpoint; also returns the id of the checkpoint holding the state it replaced.
fn restore_with_undo(
    root: &Path,
    id: &str,
    label: &str,
) -> anyhow::Result<(String, Option<String>)> {
    git::restore_checkpoint(root, id)?;
    let saved = format!("Before restoring checkpoint {id}");
    let undo = git::checkpoints(root)
        .ok()
        .and_then(|list| list.into_iter().find(|(_, l)| *l == saved))
        .map(|(n, _)| n);
    Ok((format!("Restored \"{label}\""), undo))
}

fn snapshot(root: &Path) -> anyhow::Result<GitSnapshot> {
    if !git::is_repo(root) {
        return Ok(GitSnapshot {
            repo: false,
            files: vec![],
            checkpoints: vec![],
        });
    }
    Ok(GitSnapshot {
        repo: true,
        files: git::status(root)?,
        checkpoints: git::checkpoints(root)?,
    })
}

/// An untracked file shown as all-added lines (git diff prints nothing for it).
fn untracked_diff(root: &Path, rel: &str) -> anyhow::Result<String> {
    let path = tools::resolve(root, rel.trim_end_matches('/'))?;
    if path.is_dir() {
        return Ok("New folder".into());
    }
    let text = read_text(&path)?;
    Ok(text.lines().map(|l| format!("+{l}\n")).collect())
}

fn read_text(path: &Path) -> anyhow::Result<String> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(meta.is_file(), "Not a file");
    anyhow::ensure!(meta.len() <= EDIT_LIMIT, "File is over 128 KB");
    std::fs::read_to_string(path).map_err(|_| anyhow::anyhow!("Not a UTF-8 text file"))
}

/// Whether a status entry belongs on the staged (index) or the unstaged side.
fn on_side(f: &git::GitFile, staged: bool) -> bool {
    if staged {
        !matches!(f.index, ' ' | '?')
    } else {
        f.worktree != ' '
    }
}

fn status_color(code: char, p: &Palette) -> Color32 {
    match code {
        'A' | 'U' => p.ok,
        'D' => p.danger_text,
        'M' | 'R' | 'C' => p.number,
        _ => p.text,
    }
}

#[derive(Debug, PartialEq)]
enum LineKind {
    Add,
    Del,
    Hunk,
    Meta,
    Plain,
}
fn line_kind(line: &str) -> LineKind {
    if ["+++", "---", "diff ", "index ", "new file"]
        .iter()
        .any(|m| line.starts_with(m))
    {
        LineKind::Meta
    } else if line.starts_with("@@") {
        LineKind::Hunk
    } else if line.starts_with('+') {
        LineKind::Add
    } else if line.starts_with('-') {
        LineKind::Del
    } else {
        LineKind::Plain
    }
}

fn diff_job(diff: &str, p: &Palette) -> LayoutJob {
    let font = FontId::monospace(12.5);
    let mut job = LayoutJob::default();
    let shown = head(diff, VIEW_LIMIT);
    for line in shown.split_inclusive('\n') {
        let color = match line_kind(line) {
            LineKind::Add => p.ok,
            LineKind::Del => p.danger_text,
            LineKind::Hunk => p.accent,
            LineKind::Meta => p.muted,
            LineKind::Plain => p.text,
        };
        job.append(line, 0.0, TextFormat::simple(font.clone(), color));
    }
    if shown.len() < diff.len() {
        job.append("\n… diff truncated", 0.0, TextFormat::simple(font, p.muted));
    }
    job
}

/// The first `max` bytes of `s`, cut at a character boundary.
fn head(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
/// The last `max` bytes of `s`, cut at a character boundary.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn hash_of(value: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

fn char_index(text: &str, byte: usize) -> usize {
    text.get(..byte.min(text.len()))
        .map_or(0, |s| s.chars().count())
}

// ---- editor helpers --------------------------------------------------------------------------

/// Highlighter name for a file, or "" for plain text.
fn lang_of(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rs",
        "js" | "jsx" | "mjs" | "cjs" => "js",
        "ts" | "tsx" | "mts" => "ts",
        "py" | "pyw" => "py",
        "toml" => "toml",
        "json" | "jsonc" => "json",
        "md" | "markdown" | "mdx" => "md",
        _ => "",
    }
}

/// Headings, quotes and fenced code in Markdown, by line.
fn md_spans(code: &str) -> Vec<(usize, usize, Tok)> {
    let mut out = vec![];
    let mut at = 0;
    let mut fence = false;
    for line in code.split_inclusive('\n') {
        let t = line.trim_start();
        let tok = if t.starts_with("```") {
            fence = !fence;
            Tok::Str
        } else if fence {
            Tok::Str
        } else if t.starts_with('#') {
            Tok::Keyword
        } else if t.starts_with('>') {
            Tok::Comment
        } else {
            Tok::Plain
        };
        out.push((at, at + line.len(), tok));
        at += line.len();
    }
    out
}

fn spans_for(code: &str, lang: &str) -> Vec<(usize, usize, Tok)> {
    match lang {
        "" => vec![(0, code.len(), Tok::Plain)],
        "md" => md_spans(code),
        _ => highlight(code, lang),
    }
}

/// Case-insensitive (ASCII) byte ranges of `query` in `text`.
fn find_matches(text: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return vec![];
    }
    let needle = query.to_ascii_lowercase();
    text.to_ascii_lowercase()
        .match_indices(&needle)
        .take(MAX_MATCHES)
        .map(|(i, m)| (i, i + m.len()))
        .collect()
}

/// Highlighted code with find matches given a background; `current` is drawn in the accent.
fn editor_job(
    code: &str,
    lang: &str,
    marks: &[(usize, usize)],
    current: usize,
    p: &Palette,
) -> LayoutJob {
    let font = FontId::monospace(13.0);
    let mut job = LayoutJob::default();
    let mut m = 0;
    for (s, e, tok) in spans_for(code, lang) {
        let color = match tok {
            Tok::Plain => p.text,
            Tok::Keyword => p.keyword,
            Tok::Str => p.string,
            Tok::Comment => p.comment,
            Tok::Number => p.number,
        };
        let mut at = s;
        while at < e {
            while marks.get(m).is_some_and(|mk| mk.1 <= at) {
                m += 1;
            }
            let (end, fg, bg) = match marks.get(m) {
                Some(&(ms, me)) if ms <= at => {
                    if m == current {
                        (me.min(e), p.on_accent, p.accent)
                    } else {
                        (me.min(e), color, p.selection)
                    }
                }
                Some(&(ms, _)) => (ms.min(e), color, Color32::TRANSPARENT),
                None => (e, color, Color32::TRANSPARENT),
            };
            job.append(
                &code[at..end],
                0.0,
                TextFormat {
                    font_id: font.clone(),
                    color: fg,
                    background: bg,
                    ..Default::default()
                },
            );
            at = end;
        }
    }
    job
}

/// Line edits turning `old` into `new`: (' ' | '-' | '+', line).
fn line_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<(char, &'a str)> {
    let pre = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - pre;
    let suf = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    let (a, b) = (&old[pre..old.len() - suf], &new[pre..new.len() - suf]);
    let mut ops: Vec<(char, &str)> = old[..pre].iter().map(|l| (' ', *l)).collect();
    if a.len().saturating_mul(b.len()) > 4_000_000 {
        // Too large for the table: show the changed middle as a block replacement.
        ops.extend(a.iter().map(|l| ('-', *l)));
        ops.extend(b.iter().map(|l| ('+', *l)));
    } else {
        // Longest-common-subsequence lengths of the suffixes a[i..], b[j..].
        let w = b.len() + 1;
        let mut t = vec![0u32; (a.len() + 1) * w];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                t[i * w + j] = if a[i] == b[j] {
                    t[(i + 1) * w + j + 1] + 1
                } else {
                    t[(i + 1) * w + j].max(t[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                ops.push((' ', a[i]));
                i += 1;
                j += 1;
            } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                ops.push(('-', a[i]));
                i += 1;
            } else {
                ops.push(('+', b[j]));
                j += 1;
            }
        }
        ops.extend(a[i..].iter().map(|l| ('-', *l)));
        ops.extend(b[j..].iter().map(|l| ('+', *l)));
    }
    ops.extend(old[old.len() - suf..].iter().map(|l| (' ', *l)));
    ops
}

/// A unified diff with `context` lines around each change; empty when nothing changed.
fn unified(old: &str, new: &str, context: usize) -> String {
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let ops = line_ops(&o, &n);
    let mut show = vec![false; ops.len()];
    for (i, _) in ops.iter().enumerate().filter(|(_, (k, _))| *k != ' ') {
        let end = (i + context + 1).min(ops.len());
        show[i.saturating_sub(context)..end].fill(true);
    }
    let mut out = String::new();
    let (mut old_line, mut new_line, mut shown) = (1, 1, false);
    for (i, (kind, text)) in ops.iter().enumerate() {
        if show[i] {
            if !shown {
                out.push_str(&format!("@@ -{old_line} +{new_line} @@\n"));
            }
            out.push(*kind);
            out.push_str(text);
            out.push('\n');
        }
        shown = show[i];
        match kind {
            '-' => old_line += 1,
            '+' => new_line += 1,
            _ => {
                old_line += 1;
                new_line += 1;
            }
        }
    }
    out
}

// ---- quick open and terminal helpers ---------------------------------------------------------

/// Subsequence match score (higher is better), or None when `query` does not match.
/// Consecutive letters, segment starts and matches in the file name score extra.
fn fuzzy_score(query: &str, path: &str) -> Option<i32> {
    let mut wanted = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .peekable();
    let name_start = path.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let (mut score, mut last, mut prev) = (0, None::<usize>, None::<char>);
    for (n, (i, c)) in path.char_indices().enumerate() {
        let Some(&q) = wanted.peek() else { break };
        if c.to_lowercase().eq(std::iter::once(q)) {
            wanted.next();
            score += 1;
            if last.is_some_and(|l| l + 1 == n) {
                score += 5;
            }
            if prev.is_none_or(|p| matches!(p, '/' | '\\' | '_' | '-' | '.' | ' ')) {
                score += 3;
            }
            if i >= name_start {
                score += 2;
            }
            last = Some(n);
        }
        prev = Some(c);
    }
    wanted.peek().is_none().then_some(score)
}

/// Best matches first (ties: shorter paths); all files, in order, for an empty query.
fn fuzzy_filter(files: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.trim().is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let mut scored: Vec<(i32, &String)> = files
        .iter()
        .filter_map(|f| fuzzy_score(query, f).map(|s| (s, f)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, f)| f.clone())
        .collect()
}

/// Splits an argument line at spaces; double quotes group words, `\"` is a literal quote.
fn split_args(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut any) = (vec![], String::new(), false, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
                any = true;
            }
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}
fn join_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty() || a.contains(char::is_whitespace) || a.contains('"') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

impl Studio {
    pub(super) fn workbench_page(&mut self, ui: &mut egui::Ui) {
        self.sync_workspace();
        self.workbench_poll();
        let wb = &mut self.workbench;
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Workbench").strong());
            ui.add_space(12.0);
            for (tab, label, hover) in [
                (WbTab::Git, "Git", "Changes and checkpoints"),
                (WbTab::Editor, "Editor", "Ctrl+P opens a file"),
                (
                    WbTab::Terminal,
                    "Terminal",
                    "Task profiles and their output",
                ),
            ] {
                ui.selectable_value(&mut wb.tab, tab, label)
                    .on_hover_text(hover);
            }
        });
        ui.add_space(6.0);
        match self.workbench.tab {
            WbTab::Git => self.git_tab(ui),
            WbTab::Editor => self.editor_tab(ui),
            WbTab::Terminal => self.terminal_tab(ui),
        }
        let ctx = ui.ctx().clone();
        if self.workbench.busy() {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.workbench.any_running() {
            // New output wakes the UI itself; this only updates the Running/Exited label.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Ctrl+P palette over the workspace files; Enter opens the choice in the editor.
    pub(super) fn quick_open(&mut self, ctx: &egui::Context) {
        self.sync_workspace();
        let wb = &mut self.workbench;
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::P))
            && wb.quick.take().is_none()
        {
            wb.load_files();
            wb.quick = Some(QuickOpen::default());
        }
        wb.poll_files();
        let loading = wb.files_rx.is_some();
        let files = wb.files.as_deref().unwrap_or(&[]);
        let Some(q) = wb.quick.as_mut() else { return };
        if q.last.as_deref() != Some(q.query.as_str()) {
            q.results = fuzzy_filter(files, &q.query, PALETTE_ROWS);
            q.last = Some(q.query.clone());
            q.selected = 0;
        }
        let (down, up, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        let count = q.results.len();
        if count > 0 {
            if down {
                q.selected = (q.selected + 1) % count;
            }
            if up {
                q.selected = (q.selected + count - 1) % count;
            }
        }
        let mut chosen = enter.then(|| q.results.get(q.selected).cloned()).flatten();
        let width = 520.0_f32.min(ctx.content_rect().width() - 60.0).max(200.0);
        let p = pal(ctx);
        let modal = egui::Modal::new(egui::Id::new("wb_quick_open")).show(ctx, |ui| {
            ui.set_width(width);
            let r = ui.add(
                egui::TextEdit::singleline(&mut q.query)
                    .hint_text("Go to file")
                    .desired_width(f32::INFINITY),
            );
            r.request_focus();
            if loading && files.is_empty() {
                busy(ui);
            }
            egui::ScrollArea::vertical()
                .max_height(360.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (i, path) in q.results.iter().enumerate() {
                        let resp = ui.add(
                            egui::Button::selectable(
                                i == q.selected,
                                RichText::new(path).size(12.5),
                            )
                            .truncate(),
                        );
                        if i == q.selected && (up || down) {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            chosen = Some(path.clone());
                        }
                    }
                    if q.results.is_empty() && !loading {
                        ui.label(RichText::new("No matches").color(p.muted));
                    }
                });
        });
        let close = modal.should_close();
        if let Some(path) = chosen {
            self.workbench.quick = None;
            self.open_in_editor(&path);
        } else if close {
            self.workbench.quick = None;
        }
    }

    /// Opens a workspace-relative file in the Workbench editor (asks first over unsaved edits).
    pub(super) fn open_in_editor(&mut self, rel: &str) {
        self.view = WORKBENCH_VIEW;
        let wb = &mut self.workbench;
        wb.tab = WbTab::Editor;
        if wb.editor.path == rel && wb.editor.root == self.config.workspace {
            return;
        }
        self.load_editor_keeping_edits(rel);
    }

    fn load_editor(&mut self, rel: &str) {
        let root = self.config.workspace.clone();
        match tools::resolve(&root, rel).and_then(|p| read_text(&p)) {
            Ok(text) => {
                let find = std::mem::take(&mut self.workbench.editor.find);
                let find_open = self.workbench.editor.find_open;
                self.workbench.editor = EditorState {
                    root,
                    path: rel.to_owned(),
                    saved: text.clone(),
                    text,
                    find,
                    find_open,
                    ..Default::default()
                };
            }
            Err(e) => self.error = Some(format!("Could not open {rel}: {e}")),
        }
    }

    /// Loads a file into the editor. Unsaved edits are dropped at once; an Undo toast puts them back.
    fn load_editor_keeping_edits(&mut self, rel: &str) {
        let ed = &self.workbench.editor;
        let kept = ed.dirty().then(|| {
            (
                ed.root.clone(),
                ed.path.clone(),
                ed.text.clone(),
                ed.saved.clone(),
            )
        });
        self.load_editor(rel);
        // A failed load leaves the edits in place and needs no undo.
        let Some((root, path, text, saved)) = kept.filter(|_| !self.workbench.editor.dirty())
        else {
            return;
        };
        self.undo_toast(
            format!("Discarded unsaved edits to {path}"),
            Box::new(move |studio: &mut Studio| {
                studio.view = WORKBENCH_VIEW;
                studio.workbench.tab = WbTab::Editor;
                studio.workbench.editor = EditorState {
                    root,
                    path,
                    text,
                    saved,
                    ..Default::default()
                };
            }),
            Box::new(|| {}),
        );
    }

    /// Reverts one file at once. git::discard keeps a backup copy; the file's old bytes are also
    /// held here so Undo can put them back.
    fn discard_file(&mut self, path: String) {
        let root = self.config.workspace.clone();
        let previous = tools::resolve(&root, &path).ok().and_then(|full| {
            let meta = std::fs::metadata(&full).ok()?;
            (meta.is_file() && meta.len() <= UNDO_LIMIT)
                .then(|| std::fs::read(&full).ok())
                .flatten()
        });
        let kept = previous.is_some() || tools::resolve(&root, &path).is_ok_and(|f| !f.exists());
        let name = path.clone();
        self.workbench.git_op(true, {
            let path = path.clone();
            move |root| git::discard(root, &path).map(|_| format!("Discarded changes to {path}"))
        });
        if !kept {
            self.toast(format!("Discarded {name}; a backup copy was kept"));
            return;
        }
        self.undo_toast(
            format!("Discarded changes to {name}"),
            Box::new(move |studio: &mut Studio| {
                let result = tools::resolve(&root, &path).and_then(|full| match previous {
                    Some(bytes) => std::fs::write(&full, bytes).map_err(Into::into),
                    None => std::fs::remove_file(&full).map_err(Into::into),
                });
                match result {
                    Ok(()) => {
                        studio.status = format!("Restored {path}");
                        studio.workbench.refresh();
                        studio.workbench.load_diff();
                        let ed = &studio.workbench.editor;
                        if ed.path == path && ed.root == root && !ed.dirty() {
                            studio.load_editor(&path);
                        }
                    }
                    Err(e) => studio.error_toast(format!("Could not undo: {e:#}")),
                }
            }),
            Box::new(|| {}),
        );
    }

    /// Resets per-workspace state when the workspace folder changes (terminals keep running).
    fn sync_workspace(&mut self) {
        let wb = &mut self.workbench;
        if wb.root == self.config.workspace {
            return;
        }
        wb.root = self.config.workspace.clone();
        wb.repo = None;
        wb.git_error = None;
        wb.changes.clear();
        wb.checkpoints.clear();
        wb.selected = None;
        wb.diff.clear();
        wb.diff_job = None;
        wb.pending.clear();
        wb.last_refresh = None;
        wb.files = None;
        wb.files_rx = None;
        wb.hits = None;
    }

    fn workbench_poll(&mut self) {
        let wb = &mut self.workbench;
        wb.poll_files();
        let mut msgs = vec![];
        wb.pending.retain(|rx| match rx.try_recv() {
            Ok(m) => {
                msgs.push(m);
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        let mut reload = false;
        let mut undo_restores: Vec<(String, String)> = vec![];
        for msg in msgs {
            match msg {
                GitMsg::Status(Ok(s)) => {
                    wb.repo = Some(s.repo);
                    wb.changes = s.files;
                    wb.checkpoints = s.checkpoints;
                    let gone = wb.selected.as_ref().is_some_and(|(path, staged)| {
                        !wb.changes
                            .iter()
                            .any(|f| f.path == *path && on_side(f, *staged))
                    });
                    if gone {
                        wb.selected = None;
                        wb.diff.clear();
                        wb.diff_job = None;
                    }
                }
                GitMsg::Status(Err(e)) => {
                    wb.repo = Some(wb.repo.unwrap_or(true));
                    wb.git_error = Some(e);
                }
                GitMsg::Diff(path, staged, diff) => {
                    if wb.selected.as_ref() == Some(&(path, staged)) {
                        wb.diff = diff.unwrap_or_else(|e| e);
                        wb.diff_job = None;
                    }
                }
                GitMsg::Done(result, files_changed) => {
                    match result {
                        Ok(m) => self.status = m,
                        Err(e) => self.error = Some(format!("Git: {e}")),
                    }
                    wb.refresh();
                    wb.load_diff();
                    reload |= files_changed;
                }
                GitMsg::Restored(result) => {
                    match result {
                        Ok((m, undo)) => {
                            if let Some(id) = undo {
                                undo_restores.push((id, m.clone()));
                            }
                            self.status = m;
                        }
                        Err(e) => self.error = Some(format!("Git: {e}")),
                    }
                    wb.refresh();
                    wb.load_diff();
                    reload = true;
                }
            }
        }
        for (id, message) in undo_restores {
            self.undo_toast(
                message,
                Box::new(move |studio: &mut Studio| {
                    studio.workbench.git_op(true, move |root| {
                        git::restore_checkpoint(root, &id)
                            .map(|_| "Restored the previous state".to_owned())
                    });
                }),
                Box::new(|| {}),
            );
        }
        let ed = &self.workbench.editor;
        if reload && !ed.path.is_empty() && !ed.dirty() && ed.root == self.config.workspace {
            let path = ed.path.clone();
            self.load_editor(&path);
        }
    }

    fn git_tab(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let wb = &mut self.workbench;
        if !wb.busy()
            && wb.git_error.is_none()
            && wb.last_refresh.is_none_or(|t| t.elapsed() > REFRESH)
        {
            wb.refresh();
        }
        match wb.repo {
            None => {
                busy(ui);
                return;
            }
            Some(false) => {
                ui.label(RichText::new("Not a git repository").color(p.muted));
                return;
            }
            Some(true) => {}
        }
        let mut act = None;
        egui::SidePanel::left("wb_git_list")
            .resizable(true)
            .default_width(270.0)
            .width_range(180.0..=480.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                right: 8,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        act = Some(GitAct::Refresh);
                    }
                    if wb.busy() {
                        busy(ui);
                    }
                });
                if let Some(e) = &wb.git_error {
                    ui.colored_label(p.danger_text, e);
                }
                egui::ScrollArea::vertical()
                    .id_salt("wb_git_files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for staged in [true, false] {
                            let rows: Vec<&git::GitFile> =
                                wb.changes.iter().filter(|f| on_side(f, staged)).collect();
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                Self::eyebrow(ui, if staged { "STAGED" } else { "CHANGES" });
                                if rows.is_empty() {
                                    return;
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let all = rows.iter().map(|f| f.path.clone()).collect();
                                        if staged {
                                            if ui.small_button("Unstage all").clicked() {
                                                act = Some(GitAct::Unstage(all));
                                            }
                                        } else if ui.small_button("Stage all").clicked() {
                                            act = Some(GitAct::Stage(all));
                                        }
                                    },
                                );
                            });
                            if rows.is_empty() {
                                ui.label(RichText::new("None").size(12.0).color(p.muted));
                            }
                            for f in rows {
                                let code = match (staged, f.worktree) {
                                    (true, _) => f.index,
                                    (false, '?') => 'U',
                                    (false, c) => c,
                                };
                                let selected = wb
                                    .selected
                                    .as_ref()
                                    .is_some_and(|(path, s)| *path == f.path && *s == staged);
                                let r = ui
                                    .add(
                                        egui::Button::selectable(
                                            selected,
                                            RichText::new(format!("{code}  {}", f.path))
                                                .monospace()
                                                .size(12.0)
                                                .color(status_color(code, &p)),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&f.path);
                                if r.double_clicked() && code != 'D' {
                                    act = Some(GitAct::Open(f.path.clone()));
                                } else if r.clicked() {
                                    act = Some(GitAct::Select(f.path.clone(), staged));
                                }
                            }
                        }
                        ui.add_space(10.0);
                        Self::eyebrow(ui, "CHECKPOINTS");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut wb.label)
                                    .hint_text("Label")
                                    .desired_width((ui.available_width() - 70.0).max(60.0)),
                            );
                            if ui
                                .button(icons::label(icons::PLUS, "Create"))
                                .on_hover_text(
                                    "Saves the working tree as a checkpoint without changing it",
                                )
                                .clicked()
                            {
                                act = Some(GitAct::Checkpoint);
                            }
                        });
                        if wb.checkpoints.is_empty() {
                            ui.label(RichText::new("None").size(12.0).color(p.muted));
                        }
                        for (id, label) in &wb.checkpoints {
                            ui.horizontal(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button(icons::label(icons::UNDO, "Restore"))
                                            .on_hover_text(
                                                "Replaces the working tree; the current state is saved as a checkpoint first",
                                            )
                                            .clicked()
                                        {
                                            act = Some(GitAct::Restore(id.clone(), label.clone()));
                                        }
                                        ui.add(
                                            egui::Label::new(RichText::new(label).size(12.5))
                                                .truncate(),
                                        )
                                        .on_hover_text(id);
                                    },
                                );
                            });
                        }
                    });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                let Some((path, staged)) = wb.selected.clone() else {
                    ui.label(RichText::new("Select a file to see its diff").color(p.muted));
                    return;
                };
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&path).strong()).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if staged {
                            if ui.button(icons::label(icons::UNDO, "Unstage")).clicked() {
                                act = Some(GitAct::Unstage(vec![path.clone()]));
                            }
                        } else {
                            if ui
                                .button(icons::label(icons::TRASH, "Discard"))
                                .on_hover_text("Reverts the file; Undo brings the changes back")
                                .clicked()
                            {
                                act = Some(GitAct::Discard(path.clone()));
                            }
                            if ui.button(icons::label(icons::PLUS, "Stage")).clicked() {
                                act = Some(GitAct::Stage(vec![path.clone()]));
                            }
                        }
                        if ui
                            .button(icons::label(icons::EDITOR, "Open"))
                            .on_hover_text("Open in the editor")
                            .clicked()
                        {
                            act = Some(GitAct::Open(path.clone()));
                        }
                    });
                });
                ui.separator();
                let dark = ui.ctx().theme() == egui::Theme::Dark;
                if wb.diff_job.as_ref().is_none_or(|(d, _)| *d != dark) {
                    wb.diff_job = Some((dark, diff_job(&wb.diff, &p)));
                }
                egui::ScrollArea::both()
                    .id_salt("wb_git_diff")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if wb.diff.is_empty() {
                            if wb.busy() {
                                busy(ui);
                            } else {
                                ui.label(RichText::new("No changes").color(p.muted));
                            }
                        } else if let Some((_, job)) = &wb.diff_job {
                            ui.add(egui::Label::new(job.clone()).extend());
                        }
                    });
            });
        match act {
            Some(GitAct::Refresh) => wb.refresh(),
            Some(GitAct::Select(path, staged)) => {
                wb.selected = Some((path, staged));
                wb.diff.clear();
                wb.diff_job = None;
                wb.load_diff();
            }
            Some(GitAct::Stage(paths)) => wb.git_op(false, move |root| {
                git::stage(root, &paths).map(|_| format!("Staged {} file(s)", paths.len()))
            }),
            Some(GitAct::Unstage(paths)) => wb.git_op(false, move |root| {
                git::unstage(root, &paths).map(|_| format!("Unstaged {} file(s)", paths.len()))
            }),
            Some(GitAct::Checkpoint) => {
                let label = match wb.label.trim() {
                    "" => "Checkpoint".to_owned(),
                    l => l.to_owned(),
                };
                wb.label.clear();
                wb.git_op(false, move |root| {
                    git::checkpoint(root, &label).map(|_| format!("Checkpoint \"{label}\" saved"))
                });
            }
            Some(GitAct::Discard(path)) => self.discard_file(path),
            Some(GitAct::Restore(id, label)) => {
                let root = wb.root.clone();
                wb.spawn(move || {
                    GitMsg::Restored(
                        restore_with_undo(&root, &id, &label).map_err(|e| e.to_string()),
                    )
                });
            }
            Some(GitAct::Open(path)) => self.open_in_editor(&path),
            None => {}
        }
    }

    fn editor_tab(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let (find_key, save_key) = ui.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::F),
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::S),
            )
        });
        let wb = &mut self.workbench;
        if wb.files.is_none() {
            wb.load_files();
        }
        let mut open = None;
        let mut save = save_key;
        let mut reload = false;
        let mut palette = false;
        egui::SidePanel::left("wb_editor_files")
            .resizable(true)
            .default_width(220.0)
            .width_range(140.0..=420.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                right: 8,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut wb.filter)
                            .hint_text("Filter files")
                            .desired_width((ui.available_width() - 34.0).max(60.0)),
                    );
                    if ui
                        .small_button("🔄")
                        .on_hover_text("Reload the file list")
                        .clicked()
                    {
                        wb.load_files();
                    }
                });
                let files = wb.files.as_deref().unwrap_or(&[]);
                if wb.hits.as_ref().is_none_or(|(q, _)| *q != wb.filter) {
                    wb.hits = Some((
                        wb.filter.clone(),
                        fuzzy_filter(files, &wb.filter, LIST_ROWS),
                    ));
                }
                let hits = wb.hits.as_ref().map(|(_, h)| h.as_slice()).unwrap_or(&[]);
                if hits.is_empty() {
                    if wb.files_rx.is_some() {
                        busy(ui);
                    } else {
                        ui.label(RichText::new("No files").size(12.0).color(p.muted));
                    }
                }
                let row = 20.0;
                ui.spacing_mut().item_spacing.y = 2.0;
                egui::ScrollArea::vertical()
                    .id_salt("wb_editor_list")
                    .auto_shrink([false, false])
                    .show_rows(ui, row, hits.len(), |ui, range| {
                        for path in &hits[range] {
                            let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
                            let current = wb.editor.path == *path;
                            let r = ui
                                .add_sized(
                                    [ui.available_width(), row],
                                    egui::Button::selectable(
                                        current,
                                        RichText::new(name).size(12.0),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(path);
                            if r.clicked() {
                                open = Some(path.clone());
                            }
                        }
                    });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                let ed = &mut wb.editor;
                if ed.path.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("No file open").color(p.muted));
                        if ui.button("Open…").on_hover_text("Ctrl+P").clicked() {
                            palette = true;
                        }
                    });
                    return;
                }
                if find_key {
                    ed.find_open = true;
                    ed.focus_find = true;
                    ed.show_diff = false;
                }
                let dirty = ed.dirty();
                ui.horizontal(|ui| {
                    let name = RichText::new(&ed.path).strong();
                    ui.add(egui::Label::new(name).truncate())
                        .on_hover_text(ed.root.join(&ed.path).display().to_string());
                    if dirty {
                        dot(ui, p.accent);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled_ui(dirty, |ui| Self::primary(ui, "Save"))
                            .inner
                            .on_hover_text("Ctrl+S")
                            .clicked()
                        {
                            save = true;
                        }
                        if ui
                            .button("Reload")
                            .on_hover_text("Read the file from disk again")
                            .clicked()
                        {
                            reload = true;
                        }
                        ui.toggle_value(&mut ed.show_diff, "Diff")
                            .on_hover_text("Unsaved changes against the file on disk");
                        if ui.button("Find").on_hover_text("Ctrl+F").clicked() {
                            ed.find_open = true;
                            ed.focus_find = true;
                            ed.show_diff = false;
                        }
                        if ui.button("Open…").on_hover_text("Ctrl+P").clicked() {
                            palette = true;
                        }
                    });
                });
                let matches = if ed.find_open {
                    if ed
                        .matches
                        .as_ref()
                        .is_none_or(|(rev, find, _)| *rev != ed.rev || *find != ed.find)
                    {
                        let found = Arc::new(find_matches(&ed.text, &ed.find));
                        ed.matches = Some((ed.rev, ed.find.clone(), found));
                    }
                    ed.matches
                        .as_ref()
                        .map(|(_, _, m)| m.clone())
                        .unwrap_or_default()
                } else {
                    Arc::default()
                };
                if ed.find_open && !ed.show_diff {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut ed.find)
                                .hint_text("Find")
                                .desired_width(220.0),
                        );
                        if std::mem::take(&mut ed.focus_find) {
                            r.request_focus();
                        }
                        if r.changed() {
                            ed.current = 0;
                            ed.jump = true;
                        }
                        let (enter, escape, shift) = ui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.key_pressed(egui::Key::Escape),
                                i.modifiers.shift,
                            )
                        });
                        let enter = r.lost_focus() && enter;
                        let n = matches.len();
                        let count = match n {
                            0 if ed.find.is_empty() => String::new(),
                            0 => "No matches".into(),
                            n => format!("{}/{n}", ed.current.min(n - 1) + 1),
                        };
                        ui.label(RichText::new(count).size(12.0).color(p.muted));
                        let prev = ui
                            .small_button("Prev")
                            .on_hover_text("Shift+Enter")
                            .clicked()
                            || (enter && shift);
                        let next = ui.small_button("Next").on_hover_text("Enter").clicked()
                            || (enter && !shift);
                        if enter {
                            r.request_focus();
                        }
                        if n > 0 && (prev || next) {
                            ed.current = if next {
                                (ed.current + 1) % n
                            } else {
                                (ed.current + n - 1) % n
                            };
                            ed.jump = true;
                        }
                        if ui.small_button("×").on_hover_text("Close (Esc)").clicked()
                            || (r.lost_focus() && escape)
                        {
                            ed.find_open = false;
                        }
                    });
                }
                if ed.current >= matches.len() {
                    ed.current = 0;
                }
                let dark = ui.ctx().theme() == egui::Theme::Dark;
                if ed.show_diff {
                    if ed.diff.as_ref().is_none_or(|(rev, _)| *rev != ed.rev) {
                        ed.diff = Some((ed.rev, unified(&ed.saved, &ed.text, 3)));
                        ed.diff_job = None;
                    }
                    let diff = ed.diff.as_ref().map(|(_, d)| d.as_str()).unwrap_or("");
                    if ed.diff_job.as_ref().is_none_or(|(d, _)| *d != dark) {
                        ed.diff_job = Some((dark, diff_job(diff, &p)));
                    }
                    egui::ScrollArea::both()
                        .id_salt("wb_editor_diff")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if diff.is_empty() {
                                ui.label(RichText::new("No unsaved changes").color(p.muted));
                            } else if let Some((_, job)) = &ed.diff_job {
                                ui.add(egui::Label::new(job.clone()).extend());
                            }
                        });
                    return;
                }
                let lang = lang_of(&ed.path);
                let find = if ed.find_open {
                    ed.find.clone()
                } else {
                    String::new()
                };
                let current = ed.current;
                let jump = std::mem::take(&mut ed.jump)
                    .then(|| matches.get(current).copied())
                    .flatten();
                let size = ui.available_size();
                let cache = &mut ed.galley;
                let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                    let text = egui::TextBuffer::as_str(buf);
                    let scale = ui.ctx().pixels_per_point();
                    let key = hash_of((&find, current, dark, wrap.to_bits(), scale.to_bits()));
                    // A text compare is a memcmp, far cheaper than hashing and laying out again.
                    if let Some((k, galley)) = cache.as_ref()
                        && *k == key
                        && galley.job.text == text
                    {
                        return galley.clone();
                    }
                    let marks = find_matches(text, &find);
                    let mut job = editor_job(text, lang, &marks, current, &p);
                    job.wrap.max_width = wrap;
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    *cache = Some((key, galley.clone()));
                    galley
                };
                egui::ScrollArea::vertical()
                    .id_salt("wb_editor_text")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let id = egui::Id::new("wb_editor_text");
                        if let Some((start, end)) = jump {
                            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id)
                                .unwrap_or_default();
                            state.cursor.set_char_range(Some(CCursorRange::two(
                                CCursor::new(char_index(&ed.text, start)),
                                CCursor::new(char_index(&ed.text, end)),
                            )));
                            state.store(ui.ctx(), id);
                        }
                        let out = egui::TextEdit::multiline(&mut ed.text)
                            .id(id)
                            .code_editor()
                            .lock_focus(true)
                            .desired_width(f32::INFINITY)
                            .min_size(size - Vec2::splat(4.0))
                            .layouter(&mut layouter)
                            .show(ui);
                        if out.response.changed() {
                            ed.rev += 1;
                        }
                        if let Some((start, _)) = jump {
                            let at = CCursor::new(char_index(&ed.text, start));
                            let rect = out
                                .galley
                                .pos_from_cursor(at)
                                .translate(out.galley_pos.to_vec2());
                            ui.scroll_to_rect(rect.expand(24.0), Some(egui::Align::Center));
                        }
                    });
            });
        if palette {
            wb.load_files();
            wb.quick = Some(QuickOpen::default());
        }
        if let Some(path) = open {
            self.open_in_editor(&path);
        }
        if reload {
            let path = self.workbench.editor.path.clone();
            self.load_editor_keeping_edits(&path);
        }
        if save && self.workbench.editor.dirty() {
            self.save_editor();
        }
    }

    /// Writes the editor buffer through tools::write: refused when the file changed on disk
    /// since it was opened; the previous content goes to the backups folder.
    fn save_editor(&mut self) {
        let ed = &mut self.workbench.editor;
        match tools::write(&ed.root, &ed.path, Some(&ed.saved), &ed.text) {
            Ok(message) => {
                ed.saved = ed.text.clone();
                ed.diff = None;
                self.status = message;
                if self.workbench.repo == Some(true) {
                    self.workbench.refresh();
                }
            }
            Err(e) => {
                self.error = Some(format!(
                    "Could not save {}: {e}. Use Reload to load the file from disk.",
                    ed.path
                ))
            }
        }
    }

    fn terminal_tab(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let mut run = None;
        let mut remove = None;
        let mut close = None;
        let profiles = &mut self.config.task_profiles;
        let wb = &mut self.workbench;
        egui::SidePanel::left("wb_tasks")
            .resizable(true)
            .default_width(240.0)
            .width_range(180.0..=420.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                right: 8,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                ui.horizontal(|ui| {
                    Self::eyebrow(ui, "TASKS");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icons::label(icons::PLUS, "Add")).clicked() {
                            profiles.push(terminal::TaskProfile {
                                name: format!("Task {}", profiles.len() + 1),
                                program: String::new(),
                                args: vec![],
                            });
                            wb.profile = Some(profiles.len() - 1);
                            wb.args_text.clear();
                        }
                    });
                });
                if profiles.is_empty() {
                    ui.label(RichText::new("No tasks").size(12.0).color(p.muted));
                }
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, t) in profiles.iter().enumerate() {
                    let r = ui
                        .add(
                            egui::Button::selectable(wb.profile == Some(i), t.name.as_str())
                                .truncate(),
                        )
                        .on_hover_text(format!("{} {}", t.program, join_args(&t.args)));
                    if r.clicked() {
                        wb.profile = Some(i);
                        wb.args_text = join_args(&t.args);
                    }
                    if r.double_clicked() {
                        run = Some(i);
                    }
                }
                ui.spacing_mut().item_spacing.y = 6.0;
                let Some(i) = wb.profile.filter(|&i| i < profiles.len()) else {
                    return;
                };
                ui.separator();
                let t = &mut profiles[i];
                let field = |ui: &mut egui::Ui, label: &str| {
                    ui.label(RichText::new(label).size(12.0).color(p.muted));
                };
                field(ui, "Name");
                ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(f32::INFINITY));
                field(ui, "Program");
                ui.add(
                    egui::TextEdit::singleline(&mut t.program)
                        .hint_text("cargo")
                        .desired_width(f32::INFINITY),
                )
                .on_hover_text("Started directly in the workspace, without a shell");
                field(ui, "Arguments");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut wb.args_text)
                            .hint_text("test --release")
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text("Separated by spaces; quote an argument that contains spaces")
                    .changed()
                {
                    t.args = split_args(&wb.args_text);
                }
                ui.horizontal(|ui| {
                    let ready = !t.program.trim().is_empty();
                    if ui
                        .add_enabled_ui(ready, |ui| Self::primary(ui, "Run"))
                        .inner
                        .clicked()
                    {
                        run = Some(i);
                    }
                    if ui
                        .button(icons::label(icons::TRASH, "Remove"))
                        .on_hover_text("Remove this task (Undo available)")
                        .clicked()
                    {
                        remove = Some(i);
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                ..Default::default()
            }))
            .show_inside(ui, |ui| {
                if wb.sessions.is_empty() {
                    ui.label(RichText::new("No sessions").color(p.muted));
                    return;
                }
                wb.active = wb.active.min(wb.sessions.len() - 1);
                ui.horizontal_wrapped(|ui| {
                    for (i, s) in wb.sessions.iter().enumerate() {
                        let running = s.running.load(Ordering::Relaxed);
                        dot(ui, if running { p.ok } else { p.muted });
                        if ui
                            .add(egui::Button::selectable(wb.active == i, s.title.as_str()))
                            .clicked()
                        {
                            wb.active = i;
                        }
                        if ui
                            .small_button("×")
                            .on_hover_text(if running { "Stop and close" } else { "Close" })
                            .clicked()
                        {
                            close = Some(i);
                        }
                        ui.add_space(6.0);
                    }
                });
                let s = &wb.sessions[wb.active];
                let running = s.running.load(Ordering::Relaxed);
                ui.horizontal(|ui| {
                    let code = s.exit_code.lock().ok().and_then(|c| *c);
                    let state = match (running, code) {
                        (true, _) => "Running".to_owned(),
                        (false, Some(c)) => format!("Exited with code {c}"),
                        (false, None) => "Stopped".to_owned(),
                    };
                    ui.label(RichText::new(state).size(12.0).color(p.muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if running
                            && ui
                                .button("Stop")
                                .on_hover_text("Stops the process and its children")
                                .clicked()
                        {
                            s.kill_tree();
                        }
                        if ui.button("Clear").clicked() {
                            s.clear_output();
                        }
                    });
                });
                // Laid out again only when output, width or colours change.
                let generation = s.generation.load(Ordering::Acquire);
                let color = ui.visuals().text_color();
                let key = hash_of((s.id, generation, color, ui.available_width().to_bits()));
                if wb.term_galley.as_ref().is_none_or(|(k, _)| *k != key) {
                    let text = s
                        .output
                        .lock()
                        .map(|o| tail(&o, VIEW_LIMIT).to_owned())
                        .unwrap_or_default();
                    let job = LayoutJob::simple(
                        text,
                        FontId::monospace(12.5),
                        color,
                        ui.available_width(),
                    );
                    wb.term_galley = Some((key, ui.fonts_mut(|f| f.layout_job(job))));
                }
                let galley = wb.term_galley.as_ref().map(|(_, g)| g.clone());
                egui::ScrollArea::vertical()
                    .id_salt(("wb_term", s.id))
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(galley) = galley {
                            ui.add(egui::Label::new(galley).selectable(true));
                        }
                    });
            });
        if let Some(i) = remove
            && i < self.config.task_profiles.len()
        {
            let profile = self.config.task_profiles.remove(i);
            self.workbench.profile = None;
            self.persist();
            let name = profile.name.clone();
            self.undo_toast(
                format!("Removed task {name}"),
                Box::new(move |studio: &mut Studio| {
                    let at = i.min(studio.config.task_profiles.len());
                    studio.config.task_profiles.insert(at, profile);
                    studio.persist();
                }),
                Box::new(|| {}),
            );
        }
        if let Some(i) = close {
            let wb = &mut self.workbench;
            // Dropping the session stops it and what it started.
            drop(wb.sessions.remove(i));
            if wb.active > i {
                wb.active -= 1;
            }
        }
        if let Some(t) = run.and_then(|i| self.config.task_profiles.get(i).cloned()) {
            match terminal::spawn(&self.config.workspace, t.program.trim(), &t.args) {
                Ok(mut session) => {
                    session.title = t.name;
                    let ctx = ui.ctx().clone();
                    session
                        .on_output(move || ctx.request_repaint_after(Duration::from_millis(100)));
                    let wb = &mut self.workbench;
                    wb.sessions.push(session);
                    wb.active = wb.sessions.len() - 1;
                }
                Err(e) => self.error = Some(format!("Could not start {}: {e}", t.name)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, index: char, worktree: char) -> git::GitFile {
        git::GitFile {
            path: path.into(),
            index,
            worktree,
        }
    }

    #[test]
    fn status_sides() {
        assert!(on_side(&file("a", 'M', ' '), true));
        assert!(!on_side(&file("a", 'M', ' '), false));
        assert!(on_side(&file("b", 'M', 'M'), false));
        assert!(!on_side(&file("c", '?', '?'), true));
        assert!(on_side(&file("c", '?', '?'), false));
    }

    #[test]
    fn diff_lines_are_classified() {
        assert_eq!(line_kind("+++ b/x"), LineKind::Meta);
        assert_eq!(line_kind("--- a/x"), LineKind::Meta);
        assert_eq!(line_kind("@@ -1 +1 @@"), LineKind::Hunk);
        assert_eq!(line_kind("+added"), LineKind::Add);
        assert_eq!(line_kind("-gone"), LineKind::Del);
        assert_eq!(line_kind(" same"), LineKind::Plain);
    }

    #[test]
    fn unified_diff_shows_context_and_changes() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\ni\n";
        let diff = unified(old, new, 1);
        assert_eq!(diff, "@@ -3 +3 @@\n c\n-d\n+D\n e\n@@ -8 +8 @@\n h\n+i\n");
        assert!(unified(old, old, 3).is_empty());
        assert_eq!(unified("", "x", 3), "@@ -1 +1 @@\n+x\n");
    }

    #[test]
    fn line_ops_keep_common_lines() {
        let ops = line_ops(&["a", "b", "c"], &["a", "x", "c"]);
        assert_eq!(ops, vec![(' ', "a"), ('-', "b"), ('+', "x"), (' ', "c")]);
        let ops = line_ops(&["x", "a", "b"], &["a", "b", "y"]);
        assert_eq!(ops, vec![('-', "x"), (' ', "a"), (' ', "b"), ('+', "y")]);
    }

    #[test]
    fn fuzzy_prefers_file_names_and_runs() {
        assert!(fuzzy_score("wb", "src/ui/workbench.rs").is_some());
        assert!(fuzzy_score("xyz", "src/ui/workbench.rs").is_none());
        assert!(fuzzy_score("mod", "src/ui/mod.rs") > fuzzy_score("mod", "src/models/x.rs"));
        let files: Vec<String> = ["src/models/x.rs", "src/ui/mod.rs", "README.md"]
            .map(String::from)
            .into();
        assert_eq!(fuzzy_filter(&files, "mod", 10)[0], "src/ui/mod.rs");
        assert_eq!(fuzzy_filter(&files, "", 2).len(), 2);
        assert_eq!(fuzzy_filter(&files, "READ me", 10), vec!["README.md"]);
    }

    #[test]
    fn args_split_and_join() {
        assert_eq!(split_args("test --release"), vec!["test", "--release"]);
        assert_eq!(split_args(r#"run "a b" "" x"#), vec!["run", "a b", "", "x"]);
        assert_eq!(split_args(r#"say \"hi\""#), vec!["say", r#""hi""#]);
        let args: Vec<String> = ["a b", "", "plain", r#"q"t"#].map(String::from).into();
        assert_eq!(split_args(&join_args(&args)), args);
    }

    #[test]
    fn find_is_case_insensitive() {
        assert_eq!(
            find_matches("Foo foo FOO", "foo"),
            vec![(0, 3), (4, 7), (8, 11)]
        );
        assert!(find_matches("abc", "").is_empty());
        let text = "é foo";
        let (s, _) = find_matches(text, "FOO")[0];
        assert_eq!(char_index(text, s), 2);
    }

    #[test]
    fn cuts_respect_char_boundaries() {
        assert_eq!(tail("héllo", 4), "llo");
        assert_eq!(head("héllo", 2), "h");
        assert_eq!(tail("abc", 10), "abc");
    }

    #[test]
    fn editor_highlighting_covers_text() {
        let p = DARK;
        for (code, lang) in [
            ("fn main() { let s = \"é\"; } // x", "rs"),
            ("# Title\n```\ncode\n```\ntext", "md"),
            ("plain text", ""),
        ] {
            let marks = find_matches(code, "e");
            let job = editor_job(code, lang, &marks, 0, &p);
            assert_eq!(job.text, code);
        }
        assert_eq!(lang_of("src/main.RS"), "rs");
        assert_eq!(lang_of("notes.txt"), "");
        let spans = md_spans("# A\nb\n");
        assert_eq!(spans[0].2, Tok::Keyword);
        assert_eq!(spans[1].2, Tok::Plain);
    }
}

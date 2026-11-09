mod features;
mod library;
mod models;
mod views;
mod workbench;
use crate::core::personas::Persona;
use crate::core::{diagnostics, llama, setup, update};
use crate::{
    config::{self, Config, data_dir},
    docs::{Library, Source},
    engine::{self, Approval, Event},
    tools,
};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

// Dark-theme tokens, kept as constants for code that has no Context at hand.
// Theme-aware code uses `pal(ctx)` instead.
#[allow(dead_code)]
const BG: Color32 = DARK.bg;
#[allow(dead_code)]
const PANEL: Color32 = DARK.panel;
#[allow(dead_code)]
const CARD: Color32 = DARK.card;
#[allow(dead_code)]
const TEXT: Color32 = DARK.text;
#[allow(dead_code)]
const MUTED: Color32 = DARK.muted;
#[allow(dead_code)]
const ACCENT: Color32 = DARK.accent;
#[allow(dead_code)]
const BORDER: Color32 = DARK.border;
#[allow(dead_code)]
const SELECTED: Color32 = DARK.selected;
#[allow(dead_code)]
const USER_BUBBLE: Color32 = DARK.user_bubble;

/// Colour tokens for one theme.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    card: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    on_accent: Color32,
    border: Color32,
    selected: Color32,
    user_bubble: Color32,
    hover: Color32,
    active: Color32,
    selection: Color32,
    danger_bg: Color32,
    danger_text: Color32,
    ok: Color32,
    keyword: Color32,
    string: Color32,
    comment: Color32,
    number: Color32,
}
const DARK: Palette = Palette {
    bg: Color32::from_rgb(21, 14, 30),
    panel: Color32::from_rgb(15, 9, 23),
    card: Color32::from_rgb(34, 23, 46),
    text: Color32::from_rgb(227, 233, 240),
    muted: Color32::from_rgb(140, 155, 172),
    accent: Color32::from_rgb(247, 87, 108),
    on_accent: Color32::from_rgb(15, 9, 23),
    border: Color32::from_rgb(52, 34, 66),
    selected: Color32::from_rgb(58, 26, 50),
    user_bubble: Color32::from_rgb(48, 24, 42),
    hover: Color32::from_rgb(59, 35, 71),
    active: Color32::from_rgb(88, 34, 64),
    selection: Color32::from_rgb(108, 37, 68),
    danger_bg: Color32::from_rgb(70, 37, 40),
    danger_text: Color32::from_rgb(255, 195, 190),
    ok: Color32::from_rgb(120, 210, 150),
    keyword: Color32::from_rgb(247, 140, 190),
    string: Color32::from_rgb(160, 215, 140),
    comment: Color32::from_rgb(118, 128, 142),
    number: Color32::from_rgb(240, 180, 110),
};
const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(250, 247, 252),
    panel: Color32::from_rgb(242, 236, 247),
    card: Color32::from_rgb(255, 255, 255),
    text: Color32::from_rgb(30, 24, 38),
    muted: Color32::from_rgb(98, 94, 112),
    accent: Color32::from_rgb(206, 44, 78),
    on_accent: Color32::WHITE,
    border: Color32::from_rgb(222, 212, 232),
    selected: Color32::from_rgb(250, 224, 232),
    user_bubble: Color32::from_rgb(253, 236, 242),
    hover: Color32::from_rgb(236, 228, 244),
    active: Color32::from_rgb(246, 210, 222),
    selection: Color32::from_rgb(248, 196, 210),
    danger_bg: Color32::from_rgb(253, 228, 226),
    danger_text: Color32::from_rgb(150, 30, 30),
    ok: Color32::from_rgb(30, 140, 70),
    keyword: Color32::from_rgb(170, 30, 110),
    string: Color32::from_rgb(40, 120, 40),
    comment: Color32::from_rgb(120, 120, 130),
    number: Color32::from_rgb(170, 90, 10),
};
/// A custom accent as 0x01RRGGBB; 0 keeps each theme's own accent.
static CUSTOM_ACCENT: AtomicU32 = AtomicU32::new(0);
fn set_accent(rgb: [u8; 3]) {
    let packed = if rgb == config::DEFAULT_ACCENT {
        0
    } else {
        u32::from_be_bytes([1, rgb[0], rgb[1], rgb[2]])
    };
    CUSTOM_ACCENT.store(packed, Ordering::Relaxed);
}
/// The theme's palette with the chosen accent; light themes get a darker shade for contrast.
fn palette(light: bool) -> Palette {
    let mut p = if light { LIGHT } else { DARK };
    let packed = CUSTOM_ACCENT.load(Ordering::Relaxed);
    if packed != 0 {
        let [_, r, g, b] = packed.to_be_bytes();
        p.accent = accent_shade([r, g, b], light);
        p.on_accent = on_accent(p.accent);
    }
    p
}
fn accent_shade(rgb: [u8; 3], light: bool) -> Color32 {
    let f = if light { 0.83 } else { 1.0 };
    let [r, g, b] = rgb.map(|c| (f32::from(c) * f).round() as u8);
    Color32::from_rgb(r, g, b)
}
/// Dark text on light accents, white on dark ones.
fn on_accent(accent: Color32) -> Color32 {
    let luma = 0.299 * f32::from(accent.r())
        + 0.587 * f32::from(accent.g())
        + 0.114 * f32::from(accent.b());
    if luma > 150.0 {
        DARK.panel
    } else {
        Color32::WHITE
    }
}
fn pal(ctx: &egui::Context) -> Palette {
    palette(ctx.theme() == egui::Theme::Light)
}
/// Applies the accent and density to both themes.
fn apply_style(ctx: &egui::Context, accent: [u8; 3], compact: bool) {
    set_accent(accent);
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&palette(false), true));
    ctx.set_visuals_of(egui::Theme::Light, visuals(&palette(true), false));
    ctx.all_styles_mut(|s| {
        let (spacing, padding, height) = if compact {
            (5.0, Vec2::new(8.0, 3.0), 22.0)
        } else {
            (8.0, Vec2::new(10.0, 5.0), 26.0)
        };
        s.spacing.item_spacing = Vec2::splat(spacing);
        s.spacing.button_padding = padding;
        s.spacing.interact_size.y = height;
    });
}
/// A small busy indicator that animates at about 8 fps; egui's spinner repaints every frame.
fn busy(ui: &mut egui::Ui) {
    let t = ui.input(|i| i.time);
    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), egui::Sense::hover());
    let a = (t * 6.0) as f32;
    let points = (0..12)
        .map(|k| r.center() + 6.0 * Vec2::angled(a + k as f32 * 0.4))
        .collect();
    ui.painter().add(egui::Shape::line(
        points,
        Stroke::new(1.5_f32, ui.visuals().weak_text_color()),
    ));
    ui.ctx().request_repaint_after(Duration::from_millis(120));
}
fn visuals(p: &Palette, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.panel_fill = p.bg;
    // Text-field background; egui's dark default is near-black.
    v.extreme_bg_color = p.panel;
    v.window_fill = p.card;
    v.override_text_color = Some(p.text);
    v.selection.bg_fill = p.selection;
    v.hyperlink_color = p.accent;
    v.widgets.inactive.bg_fill = p.card;
    v.widgets.inactive.weak_bg_fill = p.card;
    v.widgets.hovered.bg_fill = p.hover;
    v.widgets.hovered.weak_bg_fill = p.hover;
    v.widgets.active.bg_fill = p.active;
    v.widgets.active.weak_bg_fill = p.active;
    v.window_stroke = Stroke::new(1.0_f32, p.border);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.border);
    v
}
fn theme_preference(theme: &str) -> egui::ThemePreference {
    match theme {
        "light" => egui::ThemePreference::Light,
        "system" => egui::ThemePreference::System,
        _ => egui::ThemePreference::Dark,
    }
}

/// Adds OS fonts after egui's defaults so CJK, Arabic and emoji are not boxes.
fn install_fonts(ctx: &egui::Context) {
    // Groups of alternatives: the first font of each group that loads is used, so at most
    // one CJK fallback and one symbol/emoji font are kept in memory (egui covers Latin).
    let groups: &[&[&str]] = if cfg!(windows) {
        &[
            &["C:/Windows/Fonts/seguiemj.ttf"],
            &["C:/Windows/Fonts/msyh.ttc"],
        ]
    } else if cfg!(target_os = "macos") {
        // Apple Color Emoji is a bitmap font egui cannot draw. Smallest CJK font first.
        &[&[
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
            "/System/Library/Fonts/PingFang.ttc",
        ]]
    } else {
        &[
            &[
                "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
                "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            ],
            &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"],
        ]
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut added = false;
    for group in groups {
        let Some((path, bytes)) = group.iter().find_map(|p| Some((*p, read_font(p)?))) else {
            continue;
        };
        // Leaked once at startup so egui borrows the bytes instead of copying them.
        let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        let name = format!("system:{path}");
        fonts
            .font_data
            .insert(name.clone(), Arc::new(egui::FontData::from_static(data)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
        added = true;
    }
    if added {
        ctx.set_fonts(fonts);
    }
}
/// Reads a font file of at most 64 MB whose magic number egui accepts (it panics on others).
fn read_font(path: &str) -> Option<Vec<u8>> {
    if std::fs::metadata(path).map_or(u64::MAX, |m| m.len()) > 64 << 20 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let magic: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    [[0, 1, 0, 0], *b"OTTO", *b"ttcf", *b"true"]
        .contains(&magic)
        .then_some(bytes)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Message {
    role: String,
    text: String,
    /// Sources the reply was given, so its [n] citations survive restarts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sources: Vec<Source>,
    /// A user message whose turn reached `Conversation::history` (false after a failed or
    /// stopped reply), so regenerate and edit roll back only turns that exist.
    #[serde(default = "yes")]
    in_history: bool,
    /// File names of images sent with a user message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
    #[serde(skip)]
    cache: MessageCache,
}
fn yes() -> bool {
    true
}
/// Parsed markdown and the last measured size, rebuilt when the text changes.
#[derive(Clone, Default)]
struct MessageCache {
    parsed_len: Option<usize>,
    blocks: Arc<Vec<Block>>,
    size: Vec2,
    /// Text length when `size` was measured; off-screen messages measured at the current
    /// length are skipped, and their parsed blocks are freed until they come back.
    measured_len: Option<usize>,
    /// Stand-in height for a message never laid out, as (text length, width, height), so
    /// it is not re-estimated every frame.
    estimate: Option<(usize, f32, f32)>,
    /// Byte offset just past the last blank line outside a code fence, and how many blocks
    /// the text before it parses to. A growing (streaming) reply re-parses only the rest.
    stable: (usize, usize),
}
impl Message {
    fn new(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            text: text.into(),
            ..Default::default()
        }
    }
    fn blocks(&mut self) -> Arc<Vec<Block>> {
        let len = self.text.len();
        if self.cache.parsed_len != Some(len) {
            let cache = &mut self.cache;
            // Text only ever grows by appending, so the blocks before the stable point
            // still hold; anything else (first parse, freed cache) starts over.
            let (off, count) = cache.stable;
            let grown = cache.parsed_len.is_some_and(|p| p < len)
                && off <= len
                && self.text.is_char_boundary(off)
                && (off == 0 || self.text.as_bytes()[off - 1] == b'\n')
                && count <= cache.blocks.len();
            if !grown {
                cache.blocks = Arc::default();
                cache.stable = (0, 0);
            }
            cache.stable = parse_from(&self.text, cache.stable, Arc::make_mut(&mut cache.blocks));
            cache.parsed_len = Some(len);
            cache.size = Vec2::ZERO;
        }
        self.cache.blocks.clone()
    }
    /// Frees the parsed blocks of a message that is far off-screen; they are parsed again
    /// when it comes back into view.
    fn free_blocks(&mut self) {
        if self.cache.parsed_len.is_some() {
            self.cache.blocks = Arc::default();
            self.cache.parsed_len = None;
            self.cache.stable = (0, 0);
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Conversation {
    id: String,
    title: String,
    /// Unix seconds of the last save.
    updated: u64,
    messages: Vec<Message>,
    history: Vec<Value>,
}
/// Sidebar entry for a saved conversation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ConvMeta {
    id: String,
    title: String,
    updated: u64,
}
enum SystemEvent {
    Models(Result<Vec<String>, String>),
    Started(Result<(), String>),
    Embeddings(Vec<String>),
    Llama(Result<u16, String>),
    Setup(Result<setup::SetupResult, String>),
    UpdateChecked(Result<Option<update::Release>, String>, bool),
    UpdateStaged(Result<PathBuf, String>),
    ServerVersion(PathBuf, Option<String>),
    /// Workspace listing for the given workspace, and the same files grouped by folder
    /// (grouped on the worker thread, so a large workspace does not stall the frame).
    Files(PathBuf, Vec<String>, Vec<(String, Vec<(String, String)>)>),
    Metrics(Option<llama::Metrics>),
    /// MCP server name and its tool names, from Settings → Test.
    McpTest(String, Result<Vec<String>, String>),
    Transcribed(Result<String, String>),
    SaveFailed(String),
}
/// Sends system events from worker threads and wakes the UI, which otherwise sleeps.
#[derive(Clone)]
struct Notify {
    tx: mpsc::Sender<SystemEvent>,
    ctx: egui::Context,
}
impl Notify {
    fn send(&self, event: SystemEvent) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }
}
/// Work for the single file-writer thread, so saves stay ordered and off the UI thread.
enum WriteJob {
    Write(PathBuf, Payload),
    Flush(mpsc::Sender<()>),
}
/// What a write puts on disk: ready bytes, or a conversation encoded on the writer thread
/// (a long or image-heavy chat is several MB of JSON, too slow to encode on the frame).
enum Payload {
    Bytes(Vec<u8>),
    Conversation(Box<Conversation>),
}
impl From<Vec<u8>> for Payload {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}
impl Payload {
    fn encode(self) -> Result<Vec<u8>, String> {
        match self {
            Self::Bytes(bytes) => Ok(bytes),
            Self::Conversation(c) => {
                serde_json::to_vec(&*c).map_err(|e| format!("Could not save the conversation: {e}"))
            }
        }
    }
}
/// An image attached to the next message.
struct Attachment {
    name: String,
    mime: String,
    bytes: Vec<u8>,
}
/// A code block's "Apply to file" write waiting for the review decision.
struct ApplyPending {
    reply: Receiver<bool>,
    path: String,
    old: Option<String>,
    new: String,
}
struct SetupJob {
    progress: Arc<Mutex<setup::SetupProgress>>,
    cancel: Arc<AtomicBool>,
}
/// A dismissible notice with an optional Undo for a deleted conversation.
struct Undo {
    text: String,
    conversation: Option<Conversation>,
}

pub struct Studio {
    config: Config,
    library: Library,
    /// 0 Chat, 1 Library, 2 Connections, 3 Settings, 4 Characters, 5 Models, 6 Workbench.
    view: usize,
    coding: bool,
    use_docs: bool,
    pack: String,
    /// Names of the enabled documentation sources, kept by the Library page.
    pub(super) packs: Vec<String>,
    draft: String,
    conversation: Conversation,
    conversations: Vec<ConvMeta>,
    conversation_search: String,
    renaming: Option<(String, String)>,
    undo: Option<Undo>,
    models: Vec<String>,
    connection: String,
    online: bool,
    checking: bool,
    system_rx: Receiver<SystemEvent>,
    system_tx: mpsc::Sender<SystemEvent>,
    worker: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    approval: Option<Approval>,
    sources: Vec<Source>,
    activity: Vec<String>,
    status: String,
    files: Vec<String>,
    file_groups: Vec<(String, Vec<(String, String)>)>,
    /// Folders expanded in the sidebar file list.
    open_dirs: std::collections::HashSet<String>,
    preview: Option<(String, String)>,
    /// The preview text laid out once: (name, text length, galley).
    preview_galley: Option<(String, usize, Arc<egui::Galley>)>,
    /// The approval detail laid out once: (title, detail length, galley).
    approval_galley: Option<(String, usize, Arc<egui::Galley>)>,
    error: Option<String>,
    /// Recovery notices from damaged settings or conversation files.
    notice: Option<String>,
    embedding_models: Vec<String>,
    use_embeddings: bool,
    use_web: bool,
    active_persona: usize,
    group_chat: bool,
    pending_characters: std::collections::VecDeque<Persona>,
    group_question: String,
    /// The running reply is a later character in a group round.
    round_follow: bool,
    /// Prompt-cache save (port, prefix, reply end) waiting for an idle moment.
    pending_save: Option<(u16, String, Instant)>,
    pub(super) discord_sessions: Vec<crate::integrations::discord::Discord>,
    pub(super) discord_tokens: Vec<String>,
    pub(super) discord_webhooks: Vec<String>,
    llama: crate::core::llama::Runtime,
    llama_busy: bool,
    llama_crash: Option<String>,
    crash_reported: bool,
    server_found: Option<PathBuf>,
    server_version: Option<(PathBuf, Option<String>)>,
    version_probe: bool,
    setup: Option<SetupJob>,
    recommended: &'static setup::ModelChoice,
    ram_gb: Option<u64>,
    update: Option<update::Release>,
    update_hidden: bool,
    update_checking: bool,
    update_status: String,
    update_progress: Option<Arc<Mutex<update::UpdateProgress>>>,
    /// A downloaded update waiting for the current reply to finish.
    staged_update: Option<PathBuf>,
    close_requested: bool,
    selected_message: Option<usize>,
    editing: Option<(usize, String)>,
    fitted: bool,
    auto_started: bool,
    last_tick: Instant,
    saved_config: Vec<u8>,
    theme_applied: String,
    style_applied: ([u8; 3], bool),
    ctx: egui::Context,
    writer: Option<mpsc::Sender<WriteJob>>,
    writer_thread: Option<std::thread::JoinHandle<()>>,
    pending_writes: PendingWrites,
    pub(super) library_state: library::LibraryState,
    pub(super) models_state: models::ModelsState,
    pub(super) workbench: workbench::WorkbenchState,
    /// find_server is rescanned only when llama_server changes or, while missing, every 30 s.
    server_key: PathBuf,
    last_server_scan: Instant,
    /// Cached checks for the setup list: model file, workspace folder, a documentation source.
    paths_ok: [bool; 3],
    live_port: Option<u16>,
    metrics: Option<llama::Metrics>,
    metrics_busy: bool,
    last_metrics: Instant,
    /// The numbers may have changed since the last read (server started, reply finished);
    /// while idle off the Models page, metrics are read only then.
    metrics_dirty: bool,
    attachments: Vec<Attachment>,
    transcribing: bool,
    speaker: speech::Speaker,
    /// Message being read aloud.
    speaking: Option<usize>,
    /// The last run stopped early (step limit, error or Stop) with its progress kept.
    can_continue: bool,
    /// "Apply to file": (path being typed, code).
    apply_draft: Option<(String, String)>,
    apply_pending: Option<ApplyPending>,
    mcp_busy: bool,
    mcp_test: Option<(String, String)>,
    /// One argument per line, per MCP server, while editing.
    mcp_args: Vec<String>,
    trusted_draft: String,
    /// Extra environment names for the workspace being edited, one per line.
    env_draft: (PathBuf, String),
    /// The --embedding llama-server, started from the Models page.
    pub(super) embed_runtime: llama::EmbedRuntime,
}

const SETTINGS_NOT_SAVED: &str = "Settings not saved";
const METRICS_EVERY: Duration = Duration::from_secs(5);

/// Queued-but-unwritten jobs per path, shared with the writer thread.
type PendingWrites = Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>;

/// The writer thread: writes files in order and reports failures.
fn spawn_writer(
    notify: Notify,
    pending: PendingWrites,
) -> (mpsc::Sender<WriteJob>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<WriteJob>();
    let handle = std::thread::spawn(move || {
        for job in rx {
            match job {
                WriteJob::Write(path, payload) => {
                    match payload.encode() {
                        Ok(bytes) => {
                            if let Err(e) = config::write_atomic(&path, &bytes) {
                                notify.send(SystemEvent::SaveFailed(format!(
                                    "Could not save {}: {e}",
                                    path.display()
                                )));
                            }
                        }
                        Err(e) => notify.send(SystemEvent::SaveFailed(e)),
                    }
                    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(n) = map.get_mut(&path) {
                        *n -= 1;
                        if *n == 0 {
                            map.remove(&path);
                        }
                    }
                }
                WriteJob::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    });
    (tx, handle)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
fn conversations_dir() -> PathBuf {
    data_dir().join("conversations")
}
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn conversation_path(id: &str) -> Option<PathBuf> {
    valid_id(id).then(|| conversations_dir().join(format!("{id}.json")))
}
fn new_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut id = format!("{millis:x}");
    while conversation_path(&id).is_some_and(|p| p.exists()) {
        id.push('0');
    }
    id
}
/// First line of the first user message, shortened for the sidebar.
fn title_from(messages: &[Message]) -> String {
    let first = messages
        .iter()
        .find(|m| m.role == "You")
        .and_then(|m| m.text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("New chat")
        .trim();
    let mut title: String = first.chars().take(48).collect();
    if first.chars().count() > 48 {
        title.push('…');
    }
    title
}
/// Drops the last `turns` user turns from engine history, with everything after them.
fn rollback_history(history: &mut Vec<Value>, turns: usize) {
    if turns == 0 {
        return;
    }
    let users: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, _)| i)
        .collect();
    match users.len().checked_sub(turns) {
        Some(keep) => history.truncate(users[keep]),
        None => history.clear(),
    }
}
/// A later character in a group round: keep the round's single user turn and
/// append only this character's reply, so the next one sees every earlier reply.
fn append_group_reply(history: &mut Vec<Value>, returned: Vec<Value>) {
    if let Some(reply) = returned
        .into_iter()
        .last()
        .filter(|m| m["role"] == "assistant")
    {
        history.push(reply);
    }
}
/// Groups workspace files by folder: top-level files first, then folders A–Z.
fn group_files(files: &[String]) -> Vec<(String, Vec<(String, String)>)> {
    let mut groups: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
    for file in files {
        let normalized = file.replace('\\', "/");
        let (dir, name) = normalized
            .rsplit_once('/')
            .map(|(d, n)| (d.to_owned(), n.to_owned()))
            .unwrap_or_else(|| (String::new(), normalized.clone()));
        groups.entry(dir).or_default().push((name, file.clone()));
    }
    groups
        .into_iter()
        .map(|(dir, mut items)| {
            // Cached: one lowercase copy per file instead of two per comparison.
            items.sort_by_cached_key(|(n, _)| n.to_lowercase());
            (dir, items)
        })
        .collect()
}
fn llama_log_tail() -> String {
    let text = std::fs::read_to_string(data_dir().join("llama-server.log")).unwrap_or_default();
    let lines: Vec<_> = text.lines().rev().take(12).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Where message `index` of a conversation forks: how many messages the fork keeps and how
/// many user turns of history it drops. A fork at a user message stops before it, and that
/// message comes back as the draft.
fn fork_point(messages: &[Message], index: usize) -> (usize, usize) {
    let mut keep = (index + 1).min(messages.len());
    if keep > 0 && messages[keep - 1].role == "You" {
        keep -= 1;
    }
    let dropped = messages[keep..]
        .iter()
        .filter(|m| m.role == "You" && m.in_history)
        .count();
    (keep, dropped)
}
fn to_markdown(title: &str, messages: &[Message]) -> String {
    let mut out = format!("# {title}\n\n");
    for m in messages {
        out.push_str(&format!("**{}**\n\n{}\n\n", m.role, m.text.trim_end()));
    }
    out
}
/// A file-name-safe version of a conversation title.
fn file_stem(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    let stem = stem.trim_matches('_').to_owned();
    if stem.is_empty() { "chat".into() } else { stem }
}
/// A path named by a code block's info string ("rust src/main.rs") or first-line comment
/// ("// src/main.rs", "# file: app.py"); empty when neither names one.
fn suggested_path(lang: &str, code: &str) -> String {
    let looks_like_path = |t: &str| {
        let t = t.trim_matches(|c: char| matches!(c, '`' | '"' | '\'' | ':' | ','));
        let name = t.rsplit(['/', '\\']).next().unwrap_or(t);
        let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        (!t.contains("://")
            && !t.is_empty()
            && t.len() <= 200
            && (1..=10).contains(&ext.len())
            && ext.chars().all(|c| c.is_ascii_alphanumeric())
            && !name.starts_with('.'))
        .then(|| t.to_owned())
    };
    let info = lang
        .split([' ', ':', ',', '='])
        .skip(1)
        .find_map(&looks_like_path);
    if let Some(path) = info.or_else(|| looks_like_path(lang)) {
        return path;
    }
    let first = code.lines().next().unwrap_or("").trim();
    let Some(rest) = ["//", "#", "--", "/*", "<!--", ";"]
        .iter()
        .find_map(|p| first.strip_prefix(p))
    else {
        return String::new();
    };
    let rest = rest
        .trim()
        .trim_end_matches("*/")
        .trim_end_matches("-->")
        .trim();
    let rest = ["file:", "filename:", "path:", "file "]
        .iter()
        .find_map(|label| {
            rest.get(..label.len())
                .filter(|head| head.eq_ignore_ascii_case(label))
                .and_then(|_| rest.get(label.len()..))
        })
        .unwrap_or(rest)
        .trim();
    (!rest.contains(' '))
        .then(|| looks_like_path(rest))
        .flatten()
        .unwrap_or_default()
}
fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}
fn audio_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mpeg"),
        "flac" => Some("audio/flac"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    }
}
/// Pasted text that is only paths of image files (one per line, quoted or file:// URLs).
fn pasted_images(text: &str) -> Option<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = text
        .lines()
        .map(|l| l.trim().trim_matches('"'))
        .filter(|l| !l.is_empty())
        .map(|l| {
            let l = l.strip_prefix("file://").unwrap_or(l);
            // file:///C:/x.png → C:/x.png on Windows
            let l = if cfg!(windows) {
                l.trim_start_matches('/')
            } else {
                l
            };
            PathBuf::from(l.replace("%20", " "))
        })
        .collect();
    (!paths.is_empty() && paths.iter().all(|p| image_mime(p).is_some() && p.is_file()))
        .then_some(paths)
}
/// The progress-kept notes engine::run posts when the step limit, an error or Stop ends a run.
fn stopped_early(activity: &str) -> bool {
    (activity.starts_with("Reached the ") && activity.contains("step tool limit"))
        || activity.starts_with("Run ended early")
}
/// Monitor key for config.monitor_windows, in logical points.
fn monitor_key(monitor: Vec2, zoom: f32) -> String {
    format!(
        "{}x{}",
        (monitor.x * zoom).round() as i64,
        (monitor.y * zoom).round() as i64
    )
}
/// A saved [x, y, w, h] (logical points) that still fits the monitor, else None.
fn restore_rect(saved: [f32; 4], monitor: [f32; 2]) -> Option<[f32; 4]> {
    let [x, y, w, h] = saved;
    (saved.iter().all(|v| v.is_finite())
        && w >= 320.0
        && h >= 240.0
        && w <= monitor[0]
        && h <= monitor[1]
        && x >= -8.0
        && y >= -8.0
        && x + w <= monitor[0] + 8.0
        && y + h <= monitor[1] + 8.0)
        .then_some(saved)
}
/// A multipart/form-data body with one audio file, for /v1/audio/transcriptions.
fn multipart_audio(boundary: &str, name: &str, mime: &str, bytes: &[u8], model: &str) -> Vec<u8> {
    let name: String = name
        .chars()
        .filter(|&c| !matches!(c, '"' | '\r' | '\n' | '\\'))
        .collect();
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}
/// Sends an audio file to llama-server's transcription endpoint (needs an audio-capable model).
fn transcribe(config: &Config, name: &str, mime: &str, bytes: &[u8]) -> anyhow::Result<String> {
    let base = crate::core::provider::direct_base(config)?;
    let boundary = format!("kyttu{:x}", now_secs());
    let body = multipart_audio(&boundary, name, mime, bytes, &config.model);
    let reply: Value = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(300))
        .build()?
        .post(format!("{base}/v1/audio/transcriptions"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()?
        .json()?;
    if let Some(text) = reply["text"].as_str() {
        return Ok(text.trim().to_owned());
    }
    anyhow::bail!(
        "{}",
        reply["error"]["message"]
            .as_str()
            .unwrap_or("The server returned no transcription")
    )
}
/// Text for the OS voice: markdown markers and code blocks left out.
fn speakable(text: &str) -> String {
    let mut out = String::new();
    let mut code = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") {
            code = !code;
            continue;
        }
        if code || t.starts_with('|') {
            continue;
        }
        let t = t.trim_start_matches(['#', '>', '-', '*', '+']).trim_start();
        out.extend(t.chars().filter(|&c| !matches!(c, '*' | '`' | '_' | '#')));
        out.push('\n');
    }
    out.chars().take(6000).collect()
}

/// Read-aloud with the OS voice: PowerShell System.Speech, macOS say, spd-say or espeak.
mod speech {
    use anyhow::Context as _;
    use std::{
        io::Write,
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex},
    };
    #[derive(Clone, Default)]
    pub struct Speaker {
        child: Arc<Mutex<Option<Child>>>,
    }
    fn on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
    }
    fn command() -> anyhow::Result<Command> {
        if cfg!(windows) {
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Console]::InputEncoding = [Text.Encoding]::UTF8; Add-Type -AssemblyName System.Speech; (New-Object System.Speech.Synthesis.SpeechSynthesizer).Speak([Console]::In.ReadToEnd())",
            ]);
            Ok(cmd)
        } else if cfg!(target_os = "macos") {
            // With no message, say reads standard input.
            Ok(Command::new("say"))
        } else if on_path("spd-say") {
            let mut cmd = Command::new("spd-say");
            cmd.args(["-w", "-e"]);
            Ok(cmd)
        } else if on_path("espeak") {
            let mut cmd = Command::new("espeak");
            cmd.arg("--stdin");
            Ok(cmd)
        } else {
            anyhow::bail!("No speech program found. Install speech-dispatcher or espeak.")
        }
    }
    impl Speaker {
        /// Starts reading `text` aloud, replacing anything already speaking.
        pub fn speak(&self, text: &str) -> anyhow::Result<()> {
            self.stop();
            let text = super::speakable(text);
            if text.trim().is_empty() {
                return Ok(());
            }
            let mut cmd = command()?;
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::core::llama::quiet(&mut cmd);
            let mut child = cmd.spawn().context("Could not start the speech program")?;
            let mut stdin = child.stdin.take().context("No speech input")?;
            *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            // Closing stdin after the text tells the program to start or finish.
            std::thread::spawn(move || {
                let _ = stdin.write_all(text.as_bytes());
            });
            Ok(())
        }
        pub fn speaking(&self) -> bool {
            let mut guard = self.child.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_mut().map(Child::try_wait) {
                Some(Ok(None)) => true,
                Some(_) => {
                    *guard = None;
                    false
                }
                None => false,
            }
        }
        pub fn stop(&self) {
            let taken = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(mut child) = taken {
                let _ = child.kill();
                let _ = child.wait();
                if cfg!(all(unix, not(target_os = "macos"))) && on_path("spd-say") {
                    // Speech Dispatcher keeps talking after its client exits.
                    let _ = Command::new("spd-say").arg("-S").status();
                }
            }
        }
    }
}

fn conversation_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s != "index" && valid_id(s))
        })
        .collect()
}
fn index_path(dir: &Path) -> PathBuf {
    dir.join("index.json")
}
/// The conversation list from index.json, or from parsing every file when the index is
/// missing, damaged or out of step with the files; true when it was rebuilt.
fn list_conversations(dir: &Path) -> (Vec<ConvMeta>, bool) {
    let files = conversation_files(dir);
    let stems: std::collections::HashSet<String> = files
        .iter()
        .filter_map(|p| Some(p.file_stem()?.to_str()?.to_owned()))
        .collect();
    let indexed = std::fs::read(index_path(dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<ConvMeta>>(&b).ok())
        .filter(|list| list.len() == stems.len() && list.iter().all(|m| stems.contains(&m.id)));
    if let Some(list) = indexed {
        return (list, false);
    }
    let mut list: Vec<ConvMeta> = files
        .into_iter()
        .filter_map(|path| {
            let id = path.file_stem()?.to_str()?.to_owned();
            let meta: ConvMeta = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            Some(ConvMeta {
                title: if meta.title.is_empty() {
                    "Chat".into()
                } else {
                    meta.title
                },
                id,
                ..meta
            })
        })
        .collect();
    list.sort_by_key(|c| std::cmp::Reverse(c.updated));
    (list, true)
}

/// Loads the saved conversations, moving a pre-v3 conversation.json into the list.
fn load_conversations(config: &mut Config) -> (Vec<ConvMeta>, Conversation, Option<String>) {
    let dir = conversations_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut notice = None;
    let old = data_dir().join("conversation.json");
    if old.is_file() && conversation_files(&dir).is_empty() {
        let (loaded, note) = config::read_json::<Conversation>(&old);
        notice = note;
        if let Some(mut c) = loaded.filter(|c| !c.messages.is_empty()) {
            c.id = new_id();
            c.title = title_from(&c.messages);
            c.updated = now_secs();
            if let Some(path) = conversation_path(&c.id)
                && config::save_json(&path, &c).is_ok()
            {
                config.active_conversation = c.id.clone();
                let _ = std::fs::rename(&old, data_dir().join("conversation.migrated.json"));
            }
        }
    }
    let (list, rebuilt) = list_conversations(&dir);
    if rebuilt {
        let _ = config::save_json_compact(&index_path(&dir), &list);
    }
    let mut current = Conversation::default();
    if let Some(path) = conversation_path(&config.active_conversation) {
        let (loaded, note) = config::read_json::<Conversation>(&path);
        notice = notice.or(note);
        if let Some(mut c) = loaded {
            c.id = config.active_conversation.clone();
            current = c;
        }
    }
    if current.id.is_empty() {
        config.active_conversation.clear();
    }
    (list, current, notice)
}

impl Studio {
    pub fn new(cc: &eframe::CreationContext<'_>, config: Config, notice: Option<String>) -> Self {
        let ctx = &cc.egui_ctx;
        install_fonts(ctx);
        apply_style(ctx, config.accent, config.compact);
        ctx.set_theme(theme_preference(&config.theme));
        ctx.all_styles_mut(|s| {
            s.text_styles
                .insert(egui::TextStyle::Body, FontId::proportional(14.0));
            s.text_styles
                .insert(egui::TextStyle::Button, FontId::proportional(13.5));
            s.text_styles
                .insert(egui::TextStyle::Small, FontId::proportional(11.0));
            s.text_styles
                .insert(egui::TextStyle::Heading, FontId::proportional(19.0));
            s.text_styles
                .insert(egui::TextStyle::Monospace, FontId::monospace(13.0));
        });
        let mut config = config;
        // Saved interface scale, applied on top of the OS display scaling.
        ctx.set_zoom_factor(if config.ui_scale.is_finite() {
            config.ui_scale.clamp(0.7, 1.8)
        } else {
            1.0
        });
        let _ = std::fs::create_dir_all(data_dir());
        let library = Library::new(data_dir().join("documentation.sqlite"));
        if config.use_docs && config.use_embeddings {
            crate::knowledge::embeddings::prewarm(&config);
        }
        let (conversations, conversation, conv_notice) = load_conversations(&mut config);
        let (tx, rx) = mpsc::channel();
        let notify = Notify {
            tx: tx.clone(),
            ctx: ctx.clone(),
        };
        let pending_writes = PendingWrites::default();
        let (writer, writer_thread) = spawn_writer(notify, pending_writes.clone());
        let bots = config.discord_bots.len();
        let workspace_key = config.workspace.clone();
        let env_text = config
            .workspace_env
            .get(&workspace_key.display().to_string())
            .map(|v| v.join("\n"))
            .unwrap_or_default();
        let mut app = Self {
            library,
            view: config.last_view.min(6),
            coding: config.coding,
            use_docs: config.use_docs,
            pack: config.pack.clone(),
            packs: vec![],
            draft: String::new(),
            conversation,
            conversations,
            conversation_search: String::new(),
            renaming: None,
            undo: None,
            models: vec![],
            connection: "Checking…".into(),
            online: false,
            checking: false,
            system_rx: rx,
            system_tx: tx,
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
            approval: None,
            sources: vec![],
            activity: vec![],
            status: String::new(),
            files: vec![],
            file_groups: vec![],
            open_dirs: Default::default(),
            preview: None,
            preview_galley: None,
            approval_galley: None,
            error: None,
            notice: match (notice, conv_notice) {
                (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
                (a, b) => a.or(b),
            },
            embedding_models: vec![],
            use_embeddings: config.use_embeddings,
            use_web: config.use_web,
            active_persona: config.active_persona,
            group_chat: config.group_chat,
            pending_characters: Default::default(),
            group_question: String::new(),
            round_follow: false,
            pending_save: None,
            discord_sessions: (0..bots).map(|_| Default::default()).collect(),
            discord_tokens: vec![String::new(); bots],
            discord_webhooks: vec![String::new(); bots],
            llama: Default::default(),
            llama_busy: false,
            llama_crash: None,
            crash_reported: false,
            server_found: llama::find_server(&config),
            server_version: None,
            version_probe: false,
            setup: None,
            recommended: setup::recommended(),
            ram_gb: setup::total_ram_gb(),
            update: None,
            update_hidden: false,
            update_checking: false,
            update_status: String::new(),
            update_progress: None,
            staged_update: None,
            close_requested: false,
            selected_message: None,
            editing: None,
            fitted: false,
            auto_started: false,
            last_tick: Instant::now(),
            saved_config: vec![],
            theme_applied: config.theme.clone(),
            style_applied: (config.accent, config.compact),
            ctx: ctx.clone(),
            writer: Some(writer),
            writer_thread: Some(writer_thread),
            pending_writes,
            library_state: Default::default(),
            models_state: Default::default(),
            workbench: Default::default(),
            server_key: config.llama_server.clone(),
            last_server_scan: Instant::now(),
            paths_ok: [false; 3],
            live_port: None,
            metrics: None,
            metrics_busy: false,
            last_metrics: Instant::now(),
            metrics_dirty: false,
            attachments: vec![],
            transcribing: false,
            speaker: Default::default(),
            speaking: None,
            can_continue: false,
            apply_draft: None,
            apply_pending: None,
            mcp_busy: false,
            mcp_test: None,
            mcp_args: vec![],
            trusted_draft: config.trusted_endpoints.join("\n"),
            env_draft: (workspace_key, env_text),
            embed_runtime: Default::default(),
            config,
        };
        // saved_config starts empty, so the first tick writes migrated settings once.
        app.refresh_paths();
        app.refresh_files();
        app.check();
        // The Library page module (handle_dropped_files, every frame) starts the document
        // watcher and rebuilds the index when it does not match config.doc_sources.
        if app.config.auto_update
            && now_secs().saturating_sub(app.config.last_update_check) > 6 * 3600
        {
            app.check_updates(false);
        }
        app
    }

    fn notify(&self) -> Notify {
        Notify {
            tx: self.system_tx.clone(),
            ctx: self.ctx.clone(),
        }
    }
    /// Runs `job` on a worker thread and delivers its event, waking the UI.
    fn background(&self, job: impl FnOnce() -> SystemEvent + Send + 'static) {
        let notify = self.notify();
        std::thread::spawn(move || notify.send(job()));
    }
    /// Lists the workspace on a worker thread; the result arrives as SystemEvent::Files.
    pub(super) fn refresh_files(&mut self) {
        let root = self.config.workspace.clone();
        self.background(move || {
            let files = tools::list_ignored_aware(&root);
            let groups = group_files(&files);
            SystemEvent::Files(root, files, groups)
        });
    }
    /// Updates the cached file checks used by the setup list and Settings.
    fn refresh_paths(&mut self) {
        self.paths_ok = [
            self.config.gguf_model.is_file(),
            self.config.workspace.is_dir(),
            self.config
                .doc_sources
                .iter()
                .any(|s| s.enabled && s.path.exists()),
        ];
    }
    /// Queues a file write on the writer thread (written here if the thread is gone).
    fn queue_write(&mut self, path: PathBuf, payload: impl Into<Payload>) {
        let payload = payload.into();
        let pending = |delta: isize| {
            let mut map = self
                .pending_writes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let n = map.entry(path.clone()).or_default();
            *n = n.saturating_add_signed(delta);
            if *n == 0 {
                map.remove(&path);
            }
        };
        let unsent = match &self.writer {
            Some(writer) => {
                pending(1);
                let unsent = writer
                    .send(WriteJob::Write(path.clone(), payload))
                    .err()
                    .map(|e| e.0);
                if unsent.is_some() {
                    pending(-1);
                }
                unsent
            }
            None => Some(WriteJob::Write(path.clone(), payload)),
        };
        if let Some(WriteJob::Write(path, payload)) = unsent {
            match payload.encode() {
                Ok(bytes) => {
                    if let Err(e) = config::write_atomic(&path, &bytes) {
                        self.error = Some(format!("Could not save: {e}"));
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
    }
    /// True while a queued write of `path` has not reached the disk yet.
    fn write_pending(&self, path: &Path) -> bool {
        self.pending_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(path)
    }
    /// Waits until queued writes are on disk, before a conversation file is read or removed.
    fn flush_writes(&self) {
        if let Some(writer) = &self.writer {
            let (tx, rx) = mpsc::channel();
            if writer.send(WriteJob::Flush(tx)).is_ok() {
                let _ = rx.recv();
            }
        }
    }
    fn save_index(&mut self) {
        match serde_json::to_vec(&self.conversations) {
            Ok(bytes) => self.queue_write(index_path(&conversations_dir()), bytes),
            Err(e) => self.error = Some(format!("Could not save the chat list: {e}")),
        }
    }
    /// Copies the chat options into the config so they are saved with it.
    fn sync_options(&mut self) {
        let c = &mut self.config;
        c.coding = self.coding;
        c.use_docs = self.use_docs;
        c.use_web = self.use_web;
        c.use_embeddings = self.use_embeddings;
        c.group_chat = self.group_chat;
        c.active_persona = self.active_persona;
        c.pack.clone_from(&self.pack);
    }
    /// Writes settings when they differ from what is on disk; invalid settings are not saved.
    fn save_config(&mut self) -> anyhow::Result<()> {
        self.sync_options();
        let bytes = serde_json::to_vec(&self.config)?;
        if bytes == self.saved_config {
            return Ok(());
        }
        // Written on the writer thread: a slow or synced disk must not stall the frame.
        self.config.validate()?;
        let pretty = serde_json::to_vec_pretty(&self.config)?;
        self.queue_write(Config::path(), pretty);
        self.saved_config = bytes;
        if self
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with(SETTINGS_NOT_SAVED))
        {
            self.error = None;
        }
        Ok(())
    }
    /// Saves the open conversation (not while it is still empty). The file is written by
    /// the writer thread, compact, so a long chat does not stall the frame.
    fn save_conversation(&mut self) -> anyhow::Result<()> {
        let c = &mut self.conversation;
        if c.messages.is_empty() {
            return Ok(());
        }
        if !valid_id(&c.id) {
            c.id = new_id();
        }
        if c.title.is_empty() {
            c.title = title_from(&c.messages);
        }
        c.updated = now_secs();
        let path = conversation_path(&c.id).expect("valid id");
        // A clone (memory copies) is much cheaper than JSON encoding with escaping, which
        // runs on the writer thread instead.
        let copy = Payload::Conversation(Box::new(c.clone()));
        let meta = ConvMeta {
            id: c.id.clone(),
            title: c.title.clone(),
            updated: c.updated,
        };
        self.queue_write(path, copy);
        self.config.active_conversation = meta.id.clone();
        self.conversations.retain(|m| m.id != meta.id);
        self.conversations.insert(0, meta);
        self.save_index();
        Ok(())
    }
    fn persist(&mut self) {
        if let Err(e) = self.save_config() {
            self.error = Some(format!("{SETTINGS_NOT_SAVED}: {e:#}"));
        }
        if let Err(e) = self.save_conversation() {
            self.error = Some(format!("Could not save the conversation: {e:#}"));
        }
    }
    fn reset_chat_state(&mut self) {
        self.pending_characters.clear();
        self.sources.clear();
        self.activity.clear();
        self.error = None;
        self.selected_message = None;
        self.editing = None;
    }
    fn open_conversation(&mut self, id: &str) {
        if self.worker.is_some() || id == self.conversation.id {
            return;
        }
        self.persist();
        match self.read_conversation(id) {
            Some(mut c) => {
                c.id = id.to_owned();
                self.conversation = c;
                self.config.active_conversation = id.to_owned();
                self.reset_chat_state();
                self.view = 0;
            }
            None => self.error = Some("That conversation could not be read.".into()),
        }
    }
    /// Reads a saved conversation once queued writes have landed.
    fn read_conversation(&mut self, id: &str) -> Option<Conversation> {
        if id == self.conversation.id {
            return Some(self.conversation.clone());
        }
        let path = conversation_path(id)?;
        if self.write_pending(&path) {
            self.flush_writes();
        }
        let (loaded, note) = config::read_json::<Conversation>(&path);
        if note.is_some() {
            self.notice = note;
        }
        loaded
    }
    /// Starts a new conversation holding the messages up to `index`.
    fn fork_conversation(&mut self, index: usize) {
        if self.worker.is_some() || index >= self.conversation.messages.len() {
            return;
        }
        self.persist();
        let source = &self.conversation;
        let (keep, dropped) = fork_point(&source.messages, index);
        let draft = (keep <= index).then(|| source.messages[keep].text.clone());
        let mut history = source.history.clone();
        rollback_history(&mut history, dropped);
        let title: String = format!("{} (fork)", source.title)
            .chars()
            .take(80)
            .collect();
        self.conversation = Conversation {
            id: new_id(),
            title,
            updated: 0,
            messages: source.messages[..keep].to_vec(),
            history,
        };
        self.reset_chat_state();
        if let Some(text) = draft {
            self.draft = text;
        }
        self.persist();
        self.status = "Forked into a new chat".into();
    }
    fn toggle_pin(&mut self, id: &str) {
        let pins = &mut self.config.pinned_conversations;
        if let Some(i) = pins.iter().position(|p| p == id) {
            pins.remove(i);
        } else {
            pins.insert(0, id.to_owned());
        }
    }
    fn export_markdown(&mut self, id: &str) {
        let Some(c) = self.read_conversation(id) else {
            self.error = Some("That conversation could not be read.".into());
            return;
        };
        let title = if c.title.is_empty() {
            title_from(&c.messages)
        } else {
            c.title.clone()
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Markdown", &["md"])
            .set_file_name(format!("{}.md", file_stem(&title)))
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, to_markdown(&title, &c.messages)) {
            Ok(()) => self.status = format!("Exported to {}", path.display()),
            Err(e) => self.error = Some(format!("Could not export: {e}")),
        }
    }
    fn rename_conversation(&mut self, id: &str, title: String) {
        let title: String = title.trim().chars().take(80).collect();
        if title.is_empty() {
            return;
        }
        if id == self.conversation.id {
            self.conversation.title = title;
            self.persist();
            return;
        }
        let Some(path) = conversation_path(id) else {
            return;
        };
        let Some(mut c) = self.read_conversation(id) else {
            self.error = Some("Could not rename: the conversation could not be read".into());
            return;
        };
        c.title.clone_from(&title);
        match serde_json::to_vec(&c) {
            Ok(bytes) => {
                self.queue_write(path, bytes);
                if let Some(m) = self.conversations.iter_mut().find(|m| m.id == id) {
                    m.title = title;
                }
                self.save_index();
            }
            Err(e) => self.error = Some(format!("Could not rename: {e}")),
        }
    }
    /// Deletes a conversation, keeping a copy in conversation.prev.json for Undo.
    fn delete_conversation(&mut self, id: &str) {
        if self.worker.is_some() {
            return;
        }
        let Some(path) = conversation_path(id) else {
            return;
        };
        self.flush_writes();
        let removed = if id == self.conversation.id {
            let c = std::mem::take(&mut self.conversation);
            self.config.active_conversation.clear();
            self.reset_chat_state();
            Some(c)
        } else {
            config::read_json::<Conversation>(&path).0
        };
        if let Some(c) = &removed {
            let _ = config::save_json_compact(&data_dir().join("conversation.prev.json"), c);
        }
        let _ = std::fs::remove_file(&path);
        let mut bak = path.into_os_string();
        bak.push(".bak");
        let _ = std::fs::remove_file(bak);
        self.conversations.retain(|m| m.id != id);
        self.config.pinned_conversations.retain(|p| p != id);
        self.save_index();
        self.undo = Some(Undo {
            text: "Chat deleted".into(),
            conversation: removed,
        });
        self.persist();
    }
    fn undo_delete(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let Some(Undo {
            conversation: Some(c),
            ..
        }) = self.undo.take()
        else {
            return;
        };
        self.persist();
        self.conversation = c;
        self.reset_chat_state();
        self.persist();
    }
    fn check(&mut self) {
        if self.checking {
            return;
        }
        self.checking = true;
        let notify = self.notify();
        let config = self.config.clone();
        std::thread::spawn(move || {
            notify.send(SystemEvent::Models(
                crate::core::provider::models(&config).map_err(|e| e.to_string()),
            ));
            notify.send(SystemEvent::Embeddings(
                engine::embedding_models().unwrap_or_default(),
            ));
        });
    }
    fn start_ollama(&mut self) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.connection = "Starting Ollama…".into();
        self.background(move || {
            let result = (|| -> anyhow::Result<()> {
                let mut cmd = std::process::Command::new("ollama");
                cmd.arg("serve")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x08000000);
                }
                let _child = cmd.spawn()?;
                for _ in 0..15 {
                    if engine::models().is_ok() {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                anyhow::bail!("Ollama did not start. Check the Ollama application.")
            })()
            .map_err(|e| e.to_string());
            SystemEvent::Started(result)
        });
    }
    /// Once the monitor size is known, restores the window geometry saved for a monitor of
    /// that size. Otherwise, on high-DPI displays (e.g. 175–200% scaling on a laptop) the
    /// default or remembered size can exceed the screen, hiding the composer, so the window
    /// is shrunk and centered.
    fn fit_to_monitor(&mut self, ctx: &egui::Context) {
        if self.fitted {
            return;
        }
        let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
            return;
        };
        self.fitted = true;
        let zoom = ctx.zoom_factor();
        if let Some([x, y, w, h]) = self
            .config
            .monitor_windows
            .get(&monitor_key(monitor, zoom))
            .and_then(|r| restore_rect(*r, [monitor.x * zoom, monitor.y * zoom]))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h) / zoom));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                egui::pos2(x, y) / zoom,
            ));
            return;
        }
        let Some(outer) = ctx.input(|i| i.viewport().outer_rect) else {
            return;
        };
        let max = egui::vec2(monitor.x * 0.9, monitor.y * 0.85);
        if outer.width() <= max.x && outer.height() <= max.y {
            return;
        }
        let size = egui::vec2(max.x.min(1280.0), max.y.min(860.0));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            ((monitor.x - size.x) / 2.0).max(0.0),
            ((monitor.y - size.y) / 2.0 - 20.0).max(0.0),
        )));
    }
    /// Remembers the window size and position in logical points.
    fn remember_window(&mut self, ctx: &egui::Context) {
        if !self.fitted {
            return;
        }
        let zoom = ctx.zoom_factor();
        let (inner, outer, monitor, plain) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.inner_rect,
                v.outer_rect,
                v.monitor_size,
                v.minimized != Some(true)
                    && v.maximized != Some(true)
                    && v.fullscreen != Some(true),
            )
        });
        if !plain {
            return;
        }
        if let Some(r) = inner {
            let size = [(r.width() * zoom).round(), (r.height() * zoom).round()];
            if size[0] >= 320.0 && size[1] >= 240.0 {
                self.config.window_size = Some(size);
            }
        }
        if let Some(r) = outer {
            let pos = [(r.min.x * zoom).round(), (r.min.y * zoom).round()];
            self.config.window_pos = Some(pos);
            if let (Some(monitor), Some([w, h])) = (monitor, self.config.window_size) {
                let rect = [pos[0], pos[1], w, h];
                let key = monitor_key(monitor, zoom);
                if self.config.monitor_windows.get(&key) != Some(&rect) {
                    self.config.monitor_windows.insert(key, rect);
                }
            }
        }
    }
    fn start_llama(&mut self) {
        if self.llama_busy {
            return;
        }
        self.llama_busy = true;
        self.llama_crash = None;
        self.crash_reported = false;
        self.connection = "Loading model…".into();
        let runtime = self.llama.clone();
        let config = self.config.clone();
        self.background(move || {
            SystemEvent::Llama(runtime.start(&config).map_err(|e| format!("{e:#}")))
        });
    }
    /// Downloads llama-server and a model on a worker thread, then starts it.
    fn start_setup(&mut self) {
        if self.setup.is_some() || self.llama_busy || self.llama.running() {
            return;
        }
        self.config.setup_attempted = true;
        let job = SetupJob {
            progress: Default::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let progress = job.progress.clone();
        let cancel = job.cancel.clone();
        let mut config = self.config.clone();
        // A different model chosen in Settings replaces the current one.
        let chosen = &self.config.setup_model;
        if !chosen.is_empty()
            && config.gguf_model.file_name().and_then(|n| n.to_str()) != Some(chosen.as_str())
        {
            config.gguf_model = PathBuf::new();
        }
        self.background(move || {
            SystemEvent::Setup(
                setup::auto_setup(&config, progress, cancel).map_err(|e| format!("{e:#}")),
            )
        });
        self.setup = Some(job);
        let _ = self.save_config();
    }
    fn check_updates(&mut self, manual: bool) {
        if self.update_checking {
            return;
        }
        self.update_checking = true;
        self.config.last_update_check = now_secs();
        if manual {
            self.update_status = "Checking…".into();
        }
        self.background(move || {
            SystemEvent::UpdateChecked(update::check().map_err(|e| format!("{e:#}")), manual)
        });
    }
    fn install_update(&mut self) {
        let Some(release) = self.update.clone() else {
            return;
        };
        if self.update_progress.is_some() || self.worker.is_some() {
            return;
        }
        let progress: Arc<Mutex<update::UpdateProgress>> = Default::default();
        self.update_progress = Some(progress.clone());
        self.background(move || {
            SystemEvent::UpdateStaged(
                update::download_and_stage(&release, progress).map_err(|e| format!("{e:#}")),
            )
        });
    }
    /// True when no reply, review or group round is in progress.
    fn idle_for_update(&self) -> bool {
        self.worker.is_none() && self.approval.is_none() && self.pending_characters.is_empty()
    }
    /// Swaps in a staged update and closes; on failure brings the runtime back up.
    fn apply_update(&mut self, path: PathBuf) {
        self.persist();
        self.flush_writes();
        let was_running = self.llama.running();
        let bots = self
            .discord_sessions
            .iter()
            .any(|s| s.running.load(Ordering::Relaxed));
        // llama/ is replaced, so its server must not be running.
        for session in &self.discord_sessions {
            session.disconnect();
        }
        self.llama.stop();
        match update::apply_and_restart(&path) {
            Ok(()) => self.close_requested = true,
            Err(e) => {
                self.update_progress = None;
                if was_running {
                    self.start_llama();
                }
                let bots = if bots {
                    " Discord bots were disconnected; reconnect them on the Connections page."
                } else {
                    ""
                };
                self.error = Some(format!("Update failed: {e:#}.{bots}"));
            }
        }
    }
    /// Work done every few seconds: crash detection, server probes, autosave.
    fn tick(&mut self) {
        if self.last_tick.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.last_tick = Instant::now();
        // find_server stats every PATH entry when no server is set, so rescan only when the
        // setting changed or, while nothing is found, every 30 seconds.
        if self.config.llama_server != self.server_key
            || (self.server_found.is_none()
                && self.last_server_scan.elapsed() > Duration::from_secs(30))
        {
            self.rescan_server();
        }
        self.refresh_paths();
        if let Some(path) = self.server_found.clone()
            && !self.version_probe
            && self.server_version.as_ref().map(|(p, _)| p) != Some(&path)
        {
            self.version_probe = true;
            self.background(move || {
                let version = llama::server_version(&path);
                SystemEvent::ServerVersion(path, version)
            });
        }
        if !self.llama_busy && !self.crash_reported && self.llama.exited_unexpectedly() {
            self.crash_reported = true;
            self.llama_crash = Some(llama_log_tail());
            self.online = false;
            self.check();
        }
        // Debounced autosave: settings are written when they differ from disk.
        if let Err(e) = self.save_config() {
            self.error = Some(format!("{SETTINGS_NOT_SAVED}: {e:#}"));
        }
    }
    pub(super) fn rescan_server(&mut self) {
        self.server_key.clone_from(&self.config.llama_server);
        self.last_server_scan = Instant::now();
        self.server_found = llama::find_server(&self.config);
    }
    /// How often to read metrics: faster while the Models page shows them.
    fn metrics_every(&self) -> Duration {
        if self.view == 5 {
            Duration::from_secs(2)
        } else if self.worker.is_some() {
            METRICS_EVERY
        } else {
            Duration::from_secs(30)
        }
    }
    /// True when a metrics read can show something new: a reply is streaming, the Models
    /// page shows them, or the server started or a reply finished since the last read.
    /// While idle, tokens/s and context use cannot change.
    fn metrics_wanted(&self) -> bool {
        self.worker.is_some() || self.view == 5 || self.metrics_dirty
    }
    /// Reads tokens/s, context and RAM from the running server every few seconds.
    fn poll_metrics(&mut self) {
        if self.metrics_busy || self.last_metrics.elapsed() < self.metrics_every() {
            return;
        }
        let Some(port) = self.live_port.filter(|_| self.llama.running()) else {
            self.last_metrics = Instant::now();
            self.metrics = None;
            return;
        };
        if !self.metrics_wanted() {
            return;
        }
        self.last_metrics = Instant::now();
        self.metrics_dirty = false;
        self.metrics_busy = true;
        let runtime = self.llama.clone();
        self.background(move || SystemEvent::Metrics(runtime.metrics(port)));
    }
    fn poll(&mut self) {
        while let Ok(event) = self.system_rx.try_recv() {
            match event {
                SystemEvent::Llama(result) => {
                    self.llama_busy = false;
                    match result {
                        Ok(port) => {
                            self.live_port = Some(port);
                            // Read the first numbers right away.
                            self.last_metrics = Instant::now()
                                .checked_sub(Duration::from_secs(60))
                                .unwrap_or_else(Instant::now);
                            self.metrics_dirty = true;
                            self.config.direct_backend = true;
                            self.config.direct_url = format!("http://127.0.0.1:{port}");
                            if port != self.config.llama_port {
                                self.status = format!(
                                    "Port {} was busy; llama-server is using {port}",
                                    self.config.llama_port
                                );
                            }
                            self.config.model = llama::model_alias(&self.config);
                            self.persist();
                        }
                        Err(e) => self.error = Some(e),
                    }
                    self.check();
                }
                SystemEvent::Embeddings(models) => self.embedding_models = models,
                SystemEvent::Models(result) => {
                    self.checking = false;
                    match result {
                        Ok(models) => {
                            self.online = true;
                            self.connection = "Connected".into();
                            // Clear setup/offline notes left from before the model came up.
                            if self.worker.is_none()
                                && (self.status.starts_with("Setting up")
                                    || self.status.starts_with("Model server offline")
                                    || self.status.starts_with("Choose a GGUF"))
                            {
                                self.status.clear();
                            }
                            self.models = models;
                            if !self.models.contains(&self.config.model)
                                && let Some(first) = self.models.first()
                            {
                                self.config.model = first.clone();
                            }
                        }
                        Err(e) => {
                            self.online = false;
                            self.connection = "Offline".into();
                            self.status = format!("Model server offline: {e}");
                            if self.config.direct_backend && !self.llama_busy {
                                if self.config.gguf_model.is_file() {
                                    // llama.cpp is primary: load the saved model once at
                                    // launch when no server is already listening.
                                    if !self.auto_started {
                                        self.auto_started = true;
                                        self.start_llama();
                                    }
                                } else if !self.config.setup_attempted {
                                    // First run: download llama-server and a model once.
                                    self.start_setup();
                                    self.status = "Setting up llama.cpp…".into();
                                } else if self.setup.is_none() {
                                    self.status = "Choose a GGUF model to start.".into();
                                }
                            }
                        }
                    }
                }
                SystemEvent::Started(result) => {
                    self.checking = false;
                    if let Err(e) = result {
                        self.error = Some(e);
                    }
                    self.check();
                }
                SystemEvent::Setup(result) => {
                    let cancelled = self
                        .setup
                        .take()
                        .is_some_and(|j| j.cancel.load(Ordering::Relaxed));
                    match result {
                        Ok(done) => {
                            self.config.gguf_model = done.model;
                            if llama::find_server(&self.config).as_ref() != Some(&done.server) {
                                self.config.llama_server = done.server;
                            }
                            self.rescan_server();
                            self.refresh_paths();
                            self.persist();
                            self.start_llama();
                        }
                        Err(_) if cancelled => self.status = "Setup cancelled".into(),
                        Err(e) => self.error = Some(format!("Automatic setup failed: {e}")),
                    }
                }
                SystemEvent::UpdateChecked(result, manual) => {
                    self.update_checking = false;
                    match result {
                        Ok(Some(release)) => {
                            self.update_status =
                                format!("Version {} is available", release.version);
                            self.update = Some(release);
                            self.update_hidden = false;
                        }
                        Ok(None) => self.update_status = "Up to date".into(),
                        Err(e) => {
                            self.update_status = "Update check failed".into();
                            if manual {
                                self.error = Some(format!("Update check failed: {e}"));
                            }
                        }
                    }
                }
                SystemEvent::UpdateStaged(Ok(path)) => {
                    if self.idle_for_update() {
                        self.apply_update(path);
                    } else {
                        self.staged_update = Some(path);
                        self.status = "Update ready, restarting after this reply".into();
                    }
                }
                SystemEvent::UpdateStaged(Err(e)) => {
                    self.update_progress = None;
                    self.error = Some(format!("Update failed: {e}"));
                }
                SystemEvent::ServerVersion(path, version) => {
                    self.version_probe = false;
                    self.server_version = Some((path, version));
                }
                SystemEvent::Files(root, files, groups) => {
                    if root == self.config.workspace {
                        self.file_groups = groups;
                        self.files = files;
                    }
                }
                SystemEvent::Metrics(metrics) => {
                    self.metrics_busy = false;
                    self.metrics = metrics;
                }
                SystemEvent::McpTest(name, result) => {
                    self.mcp_busy = false;
                    let text = match result {
                        Ok(tools) if tools.is_empty() => "Connected, no tools".into(),
                        Ok(tools) => format!("{} tools: {}", tools.len(), tools.join(", ")),
                        Err(e) => format!("Failed: {e}"),
                    };
                    self.mcp_test = Some((name, text));
                }
                SystemEvent::Transcribed(result) => {
                    self.transcribing = false;
                    match result {
                        Ok(text) => {
                            if !self.draft.trim().is_empty() {
                                self.draft.push(' ');
                            }
                            self.draft.push_str(&text);
                        }
                        Err(e) => self.error = Some(format!("Transcription failed: {e}")),
                    }
                }
                SystemEvent::SaveFailed(e) => self.error = Some(e),
            }
        }
        let events: Vec<_> = self
            .worker
            .as_ref()
            .map(|r| r.try_iter().collect())
            .unwrap_or_default();
        for event in events {
            match event {
                Event::Token(token) => {
                    if let Some(m) = self.conversation.messages.last_mut() {
                        m.text.push_str(&token);
                    }
                }
                Event::Sources(sources) => {
                    let message = self.conversation.messages.last_mut();
                    let mut stored = message.map(|m| &mut m.sources);
                    for source in sources {
                        let same = |s: &Source| s.path == source.path && s.line == source.line;
                        if let Some(list) = stored.as_deref_mut()
                            && !list.iter().any(same)
                        {
                            list.push(source.clone());
                        }
                        if !self.sources.iter().any(same) {
                            self.sources.push(source);
                        }
                    }
                }
                Event::Activity(message) => {
                    if stopped_early(&message) {
                        self.can_continue = true;
                    }
                    self.status = message.clone();
                    self.activity.push(message);
                    if self.activity.len() > 100 {
                        self.activity.remove(0);
                    }
                }
                Event::Approval(approval) => {
                    self.status = "Waiting for your review".into();
                    self.approval = Some(approval);
                }
                Event::Finished(result) => {
                    self.worker = None;
                    self.approval = None;
                    match result {
                        Ok(history) => {
                            if self.round_follow {
                                append_group_reply(&mut self.conversation.history, history);
                            } else {
                                self.conversation.history = history;
                                let mut users = self.conversation.messages.iter_mut().rev();
                                if let Some(m) = users.find(|m| m.role == "You") {
                                    m.in_history = true;
                                }
                            }
                            self.status = "Complete".into();
                            // Saves the prompt KV cache once per server run (a no-op after that),
                            // deferred to an idle moment so it never delays the next request.
                            if self.config.direct_backend
                                && self.llama.running()
                                && self.pending_save.is_none()
                                && !self.llama.prefix_saved()
                                && let Some(port) = self
                                    .config
                                    .direct_url
                                    .trim_end_matches('/')
                                    .rsplit(':')
                                    .next()
                                    .and_then(|p| p.parse::<u16>().ok())
                            {
                                let prefix = format!(
                                    "{}{}",
                                    self.conversation
                                        .history
                                        .first()
                                        .map(|m| m.to_string())
                                        .unwrap_or_default(),
                                    self.coding
                                );
                                self.pending_save = Some((port, prefix, Instant::now()));
                            }
                        }
                        Err(e) => {
                            self.can_continue = false;
                            self.status = e.clone();
                            self.error = Some(e);
                        }
                    }
                    // Only Code mode can change files (the engine rejects tool calls in Chat
                    // mode, and the mode cannot change while a reply runs).
                    if self.coding {
                        self.refresh_files();
                    }
                    // One final reading for the composer after each reply.
                    self.metrics_dirty = true;
                    self.persist();
                    if self.config.tts && self.error.is_none() && self.pending_characters.is_empty()
                    {
                        let last = self.conversation.messages.len().saturating_sub(1);
                        self.speak(last);
                    }
                }
            }
        }
        if self.worker.is_none() && !self.pending_characters.is_empty() {
            if self.cancel.load(Ordering::Relaxed) || self.error.is_some() {
                self.pending_characters.clear();
            } else if let Some(persona) = self.pending_characters.pop_front() {
                self.round_follow = true;
                self.start_character(self.group_question.clone(), persona, vec![], false);
            }
        }
        if self.worker.is_none()
            && self.pending_characters.is_empty()
            && self
                .pending_save
                .as_ref()
                .is_some_and(|(_, _, at)| at.elapsed() >= Duration::from_secs(5))
            && let Some((port, prefix, _)) = self.pending_save.take()
        {
            let (rt, cfg) = (self.llama.clone(), self.config.clone());
            std::thread::spawn(move || rt.save_prefix(&cfg, port, &prefix));
        }
        if self.idle_for_update()
            && let Some(path) = self.staged_update.take()
        {
            self.apply_update(path);
        }
    }
    fn send(&mut self) {
        if self.worker.is_some() || self.draft.trim().is_empty() {
            return;
        }
        let question: String = self.draft.trim().chars().take(6000).collect();
        if self.submit(question) {
            self.draft.clear();
        }
    }
    /// Starts a turn for `question`; false when it could not start.
    fn submit(&mut self, question: String) -> bool {
        if self.worker.is_some() {
            return false;
        }
        if self.apply_pending.is_some() {
            self.error = Some("Finish the current review first".into());
            return false;
        }
        if self.group_chat && !self.coding && !self.config.personas.iter().any(|p| p.in_group) {
            self.error = Some("Create a character and include it in group chats first.".into());
            return false;
        }
        self.error = None;
        self.editing = None;
        self.can_continue = false;
        self.sources.clear();
        self.activity.clear();
        self.selected_message = None;
        let attached = std::mem::take(&mut self.attachments);
        // in_history stays false until the reply finishes successfully.
        let mut message = Message::new("You", question.clone());
        message.images = attached.iter().map(|a| a.name.clone()).collect();
        self.conversation.messages.push(message);
        let images: Vec<(String, Vec<u8>)> =
            attached.into_iter().map(|a| (a.mime, a.bytes)).collect();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.group_question = question.clone();
        self.round_follow = false;
        self.pending_characters = if self.group_chat && !self.coding {
            crate::core::personas::group_order(&self.config.personas)
                .into_iter()
                .cloned()
                .collect()
        } else {
            self.config
                .personas
                .get(self.active_persona)
                .cloned()
                .into_iter()
                .collect()
        };
        let persona = self.pending_characters.pop_front().unwrap_or_default();
        self.start_character(question, persona, images, false);
        true
    }
    /// Resumes a run that stopped early from its kept history with a "Continue." turn.
    fn continue_run(&mut self) {
        if self.worker.is_some() || self.apply_pending.is_some() || !self.can_continue {
            return;
        }
        self.can_continue = false;
        self.error = None;
        self.sources.clear();
        self.activity.clear();
        self.selected_message = None;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.pending_characters.clear();
        self.round_follow = false;
        self.conversation
            .messages
            .push(Message::new("You", "Continue."));
        let persona = self
            .config
            .personas
            .get(self.active_persona)
            .cloned()
            .unwrap_or_default();
        self.start_character("Continue.".into(), persona, vec![], true);
    }
    /// Replaces the replies to the last user message with new ones.
    fn regenerate(&mut self) {
        if self.worker.is_some() || self.apply_pending.is_some() {
            return;
        }
        let Some(i) = self
            .conversation
            .messages
            .iter()
            .rposition(|m| m.role == "You")
        else {
            return;
        };
        let question = self.conversation.messages[i].text.clone();
        let in_history = self.conversation.messages[i].in_history;
        self.conversation.messages.truncate(i);
        if in_history {
            rollback_history(&mut self.conversation.history, 1);
        }
        self.submit(question);
    }
    /// Drops the user message at `index` and everything after it, then sends `text`.
    fn resend_edited(&mut self, index: usize, text: String) {
        let text: String = text.trim().chars().take(6000).collect();
        if self.worker.is_some()
            || self.apply_pending.is_some()
            || text.is_empty()
            || index >= self.conversation.messages.len()
        {
            return;
        }
        let turns = self.conversation.messages[index..]
            .iter()
            .filter(|m| m.role == "You" && m.in_history)
            .count();
        self.conversation.messages.truncate(index);
        rollback_history(&mut self.conversation.history, turns);
        self.submit(text);
    }
    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.status = "Stopping…".into();
        if let Some(a) = self.approval.take() {
            let _ = a.reply.send(false);
        }
    }
    /// Starts one character's reply; `resume` continues a run that stopped early.
    fn start_character(
        &mut self,
        question: String,
        persona: Persona,
        images: Vec<(String, Vec<u8>)>,
        resume: bool,
    ) {
        self.speaker.stop();
        self.speaking = None;
        self.conversation
            .messages
            .push(Message::new(persona.name.clone(), String::new()));
        let mut config = self.config.clone();
        if !persona.model.is_empty() {
            config.model = persona.model.clone();
        }
        let mut request = engine::Request {
            config,
            library: self.library.clone(),
            question,
            history: self.conversation.history.clone(),
            coding: self.coding,
            use_docs: self.use_docs,
            pack: self.pack.clone(),
            cancel: self.cancel.clone(),
            persona,
            use_web: self.use_web,
            use_embeddings: self.use_embeddings,
        };
        if !images.is_empty() {
            engine::attach_images(&mut request.history, &images);
        }
        if resume {
            request = engine::continue_request(request);
        }
        // Engine events are forwarded so each one wakes the UI instead of a fixed redraw loop;
        // tokens are coalesced to about 30 redraws a second. egui subtracts the predicted
        // frame time (1/60 s) from every request_repaint_after delay, so 50 ms here is
        // about 33 ms; asking for 30 ms gave a frame for nearly every token.
        let (engine_tx, engine_rx) = mpsc::channel::<Event>();
        let (ui_tx, ui_rx) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            for event in engine_rx {
                let token = matches!(event, Event::Token(_));
                let finished = matches!(event, Event::Finished(_));
                if ui_tx.send(event).is_err() {
                    break;
                }
                if token {
                    ctx.request_repaint_after(Duration::from_millis(50));
                } else {
                    ctx.request_repaint();
                }
                if finished {
                    break;
                }
            }
        });
        self.worker = Some(ui_rx);
        engine::spawn(request, engine_tx);
    }
    /// Starts an empty conversation; the previous one stays in the sidebar.
    fn new_chat(&mut self) {
        self.persist();
        self.conversation = Conversation::default();
        self.config.active_conversation.clear();
        self.reset_chat_state();
    }
    pub(super) fn choose_workspace(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_directory(&self.config.workspace)
            .pick_folder()
        {
            self.open_workspace(path);
        }
    }
    /// Switches the workspace (a new chat starts; the old one stays in the list).
    pub(super) fn open_workspace(&mut self, path: PathBuf) {
        if self.worker.is_some() {
            return;
        }
        self.new_chat();
        self.config.remember_project(&path);
        self.config.workspace = path;
        self.files.clear();
        self.file_groups.clear();
        self.open_dirs.clear();
        self.refresh_files();
        self.refresh_paths();
        self.preview = None;
        self.persist();
    }
    fn choose_model_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("GGUF model", &["gguf"])
            .pick_file()
        {
            self.config.gguf_model = path;
            self.refresh_paths();
            self.persist();
        }
    }
    /// Reads message `index` aloud, or stops it when it is already speaking.
    fn speak(&mut self, index: usize) {
        if self.speaking == Some(index) && self.speaker.speaking() {
            self.speaker.stop();
            self.speaking = None;
            return;
        }
        let Some(text) = self
            .conversation
            .messages
            .get(index)
            .filter(|m| m.role != "You")
            .map(|m| m.text.clone())
        else {
            return;
        };
        match self.speaker.speak(&text) {
            Ok(()) => self.speaking = Some(index),
            Err(e) => self.error = Some(format!("Read aloud failed: {e:#}")),
        }
    }
    fn choose_attachments(&mut self) {
        let mut dialog =
            rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "webp"]);
        if self.config.direct_backend {
            dialog = dialog.add_filter("Audio to transcribe", &["wav", "mp3", "flac", "ogg"]);
        }
        if let Some(paths) = dialog.pick_files() {
            self.attach_paths(paths);
        }
    }
    /// Attaches images (at most 4, 10 MB each) and transcribes audio files into the draft.
    pub(super) fn attach_paths(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let size = std::fs::metadata(&path).map_or(u64::MAX, |m| m.len());
            if let Some(mime) = audio_mime(&path) {
                if size > 25 << 20 {
                    self.error = Some(format!("{name} is larger than 25 MB."));
                } else if self.transcribing {
                    self.error = Some("Wait for the current transcription.".into());
                } else if let Ok(bytes) = std::fs::read(&path) {
                    self.transcribing = true;
                    let config = self.config.clone();
                    self.background(move || {
                        SystemEvent::Transcribed(
                            transcribe(&config, &name, mime, &bytes).map_err(|e| format!("{e:#}")),
                        )
                    });
                }
                continue;
            }
            let Some(mime) = image_mime(&path) else {
                self.error = Some(format!("{name} is not a PNG, JPEG or WebP image."));
                continue;
            };
            if self.attachments.len() >= 4 {
                self.error = Some("At most 4 images per message.".into());
                break;
            }
            if size > 10 << 20 {
                self.error = Some(format!("{name} is larger than 10 MB."));
                continue;
            }
            match std::fs::read(&path) {
                Ok(bytes) => self.attachments.push(Attachment {
                    name,
                    mime: mime.into(),
                    bytes,
                }),
                Err(e) => self.error = Some(format!("Could not read {name}: {e}")),
            }
        }
    }
    /// "Apply to file": shows the change as a normal write review.
    fn review_apply(&mut self, path: String, code: String) {
        if self.worker.is_some() || self.approval.is_some() {
            self.error = Some("Finish the current review or reply first".into());
            return;
        }
        let path = path.trim().replace('\\', "/");
        let result = (|| -> anyhow::Result<(Option<String>, String)> {
            anyhow::ensure!(!path.is_empty(), "Enter a file path");
            anyhow::ensure!(code.len() <= 128_000, "The code block exceeds 128 KB");
            let resolved = tools::resolve(&self.config.workspace, &path)?;
            let old = if resolved.exists() {
                anyhow::ensure!(
                    std::fs::metadata(&resolved)?.len() <= 128_000,
                    "The existing file exceeds the 128 KB review limit"
                );
                Some(std::fs::read_to_string(&resolved)?)
            } else {
                None
            };
            let mut new = code.clone();
            if !new.ends_with('\n') {
                new.push('\n');
            }
            Ok((old, new))
        })();
        let (old, new) = match result {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(format!("Cannot apply: {e:#}"));
                return;
            }
        };
        let detail = similar::TextDiff::from_lines(old.as_deref().unwrap_or(""), new.as_str())
            .unified_diff()
            .header(&path, &path)
            .to_string();
        let (tx, rx) = mpsc::channel();
        self.approval = Some(Approval {
            title: format!("{} {path}", if old.is_some() { "Write" } else { "Create" }),
            detail,
            reply: tx,
        });
        self.apply_pending = Some(ApplyPending {
            reply: rx,
            path,
            old,
            new,
        });
    }
    /// Writes an approved "Apply to file" change.
    fn finish_apply(&mut self) {
        let Some(pending) = &self.apply_pending else {
            return;
        };
        let approved = match pending.reply.try_recv() {
            Ok(v) => v,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => false,
        };
        let Some(p) = self.apply_pending.take() else {
            return;
        };
        if !approved {
            self.status = "Change declined".into();
            return;
        }
        match tools::write(&self.config.workspace, &p.path, p.old.as_deref(), &p.new) {
            Ok(message) => {
                self.status = message;
                self.refresh_files();
            }
            Err(e) => self.error = Some(format!("Could not write {}: {e:#}", p.path)),
        }
    }
    fn test_mcp(&mut self, index: usize) {
        let Some(server) = self.config.mcp_servers.get(index).cloned() else {
            return;
        };
        if self.mcp_busy {
            return;
        }
        self.mcp_busy = true;
        self.mcp_test = Some((server.name.clone(), "Testing…".into()));
        self.background(move || {
            let result = crate::core::mcp::list_tools(&server)
                .map(|tools| {
                    tools
                        .iter()
                        .filter_map(|t| t["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .map_err(|e| format!("{e:#}"));
            SystemEvent::McpTest(server.name, result)
        });
    }
    fn any_bot_running(&self) -> bool {
        self.discord_sessions
            .iter()
            .any(|s| s.running.load(Ordering::Relaxed))
    }
    fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
        let p = pal(ui.ctx());
        egui::Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(10)
            .inner_margin(14)
            .show(ui, body);
    }
    fn eyebrow(ui: &mut egui::Ui, text: &str) {
        let p = pal(ui.ctx());
        ui.label(RichText::new(text).size(10.5).strong().color(p.muted));
    }
    fn page_title(ui: &mut egui::Ui, text: &str) {
        ui.heading(RichText::new(text).strong());
        ui.add_space(8.0);
    }
    fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
        let p = pal(ui.ctx());
        ui.add(
            egui::Button::new(RichText::new(text).color(p.on_accent).strong())
                .fill(p.accent)
                .corner_radius(8),
        )
    }
    /// A full-width notice strip; `danger` uses the error colours.
    fn banner(ui: &mut egui::Ui, danger: bool, body: impl FnOnce(&mut egui::Ui)) {
        let p = pal(ui.ctx());
        egui::Frame::new()
            .fill(if danger { p.danger_bg } else { p.card })
            .stroke(Stroke::new(
                1.0_f32,
                if danger { p.danger_bg } else { p.border },
            ))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if danger {
                    ui.visuals_mut().override_text_color = Some(p.danger_text);
                }
                body(ui);
            });
        ui.add_space(8.0);
    }
    fn banners(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = self.error.clone() {
            let mut dismiss = false;
            Self::banner(ui, true, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(error);
                    dismiss = ui.small_button("Dismiss").clicked();
                });
            });
            if dismiss {
                self.error = None;
            }
        }
        if let Some(log) = self.llama_crash.clone() {
            let (mut restart, mut dismiss) = (false, false);
            Self::banner(ui, true, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("The model server stopped unexpectedly.").strong());
                    restart = ui
                        .add_enabled(!self.llama_busy, egui::Button::new("Restart"))
                        .clicked();
                    dismiss = ui.small_button("Dismiss").clicked();
                });
                if !log.trim().is_empty() {
                    egui::CollapsingHeader::new(RichText::new("Server log").size(12.0))
                        .id_salt("crash_log")
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(&log).monospace().size(11.5))
                                    .selectable(true),
                            );
                        });
                }
            });
            if restart {
                self.start_llama();
            }
            if dismiss {
                self.llama_crash = None;
            }
        }
        if let Some(notice) = self.notice.clone() {
            let mut dismiss = false;
            Self::banner(ui, false, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(notice);
                    dismiss = ui.small_button("Dismiss").clicked();
                });
            });
            if dismiss {
                self.notice = None;
            }
        }
        if let Some(undo) = &self.undo {
            let text = undo.text.clone();
            let can_undo = undo.conversation.is_some() && self.worker.is_none();
            let (mut restore, mut dismiss) = (false, false);
            Self::banner(ui, false, |ui| {
                ui.horizontal(|ui| {
                    ui.label(text);
                    if can_undo {
                        restore = ui.small_button("Undo").clicked();
                    }
                    dismiss = ui.small_button("Dismiss").clicked();
                });
            });
            if restore {
                self.undo_delete();
            } else if dismiss {
                self.undo = None;
            }
        }
        self.update_banner(ui);
    }
    fn update_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(progress) = &self.update_progress {
            let p = progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Self::banner(ui, false, |ui| {
                ui.horizontal(|ui| {
                    busy(ui);
                    ui.label(if p.stage.is_empty() {
                        "Downloading update…"
                    } else {
                        p.stage.as_str()
                    });
                });
                if p.total > 0 {
                    ui.add(
                        egui::ProgressBar::new(p.done as f32 / p.total as f32)
                            .text(format_mb(p.done, p.total)),
                    );
                }
            });
            return;
        }
        let Some(release) = self.update.clone() else {
            return;
        };
        if self.update_hidden {
            return;
        }
        let (mut install, mut later) = (false, false);
        let blocked = update::self_update_blocked();
        Self::banner(ui, false, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Update v{} available", release.version)).strong());
                install = ui
                    .add_enabled(
                        self.worker.is_none() && blocked.is_none(),
                        egui::Button::new("Install"),
                    )
                    .on_disabled_hover_text(
                        blocked
                            .clone()
                            .unwrap_or_else(|| "Wait for the current reply to finish.".into()),
                    )
                    .clicked();
                later = ui.small_button("Later").clicked();
                let link = ui.hyperlink_to(
                    "Release notes",
                    format!(
                        "https://github.com/{}/releases/tag/{}",
                        update::REPO,
                        release.tag
                    ),
                );
                if !release.notes.trim().is_empty() {
                    let _ = link.on_hover_text(release.notes.chars().take(800).collect::<String>());
                }
            });
        });
        if install {
            self.install_update();
        }
        if later {
            self.update_hidden = true;
        }
    }
    fn setup_progress(&mut self, ui: &mut egui::Ui) {
        let Some(job) = &self.setup else {
            return;
        };
        let p = job
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut cancel = false;
        ui.horizontal(|ui| {
            busy(ui);
            ui.label(if p.stage.is_empty() {
                "Setting up…"
            } else {
                p.stage.as_str()
            });
            cancel = ui.small_button("Cancel").clicked();
        });
        if p.total > 0 {
            let fraction = (p.done as f32 / p.total as f32).clamp(0.0, 1.0);
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "{} · {:.0}%",
                format_mb(p.done, p.total),
                fraction * 100.0
            )));
        }
        if cancel {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn format_mb(done: u64, total: u64) -> String {
    format!("{} / {} MB", done / 1_048_576, total / 1_048_576)
}

/// Monospace text laid out once, so large dialogs are not copied and hashed every frame.
fn mono_galley(ctx: &egui::Context, text: &str, color: Color32) -> Arc<egui::Galley> {
    ctx.fonts_mut(|f| {
        f.layout_job(egui::text::LayoutJob::simple(
            text.to_owned(),
            FontId::monospace(13.0),
            color,
            f32::INFINITY,
        ))
    })
}

impl eframe::App for Studio {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.models_poll();
        self.tick();
        self.poll_metrics();
        self.finish_apply();
        if self.close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.theme_applied != self.config.theme {
            self.theme_applied = self.config.theme.clone();
            ctx.set_theme(theme_preference(&self.config.theme));
            self.preview_galley = None;
            self.approval_galley = None;
        }
        let style = (self.config.accent, self.config.compact);
        if self.style_applied != style {
            self.style_applied = style;
            apply_style(ctx, style.0, style.1);
        }
        if self.speaking.is_some() && !self.speaker.speaking() {
            self.speaking = None;
        }
        self.config.ui_scale = ctx.zoom_factor();
        self.config.last_view = self.view;
        self.fit_to_monitor(ctx);
        self.remember_window(ctx);
        let screen = ctx.content_rect();
        let idle = self.worker.is_none();
        let (new_chat, open, rail, escape) = ctx.input(|i| {
            let command = i.modifiers.command;
            (
                command && i.key_pressed(egui::Key::N),
                command && i.key_pressed(egui::Key::O),
                command && i.key_pressed(egui::Key::B),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if new_chat && idle {
            self.new_chat();
            self.view = 0;
        }
        if open && idle {
            self.choose_workspace();
        }
        if rail {
            self.config.sidebar_collapsed = !self.config.sidebar_collapsed;
        }
        if escape && !idle {
            self.stop();
        } else if escape {
            self.editing = None;
            self.apply_draft = None;
        }
        // Images dropped on the Chat view are attachments; everything else goes to the Library.
        if self.view == 0 {
            let images = ctx.input_mut(|i| {
                let (images, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut i.raw.dropped_files)
                    .into_iter()
                    .partition(|f| {
                        f.path
                            .as_deref()
                            .is_some_and(|p| image_mime(p).is_some() || audio_mime(p).is_some())
                    });
                i.raw.dropped_files = rest;
                images
            });
            let paths: Vec<PathBuf> = images.into_iter().filter_map(|f| f.path).collect();
            if !paths.is_empty() {
                self.attach_paths(paths);
            }
        }
        self.handle_dropped_files(ctx);
        self.schedule_repaint(ctx, idle);
        self.sidebar(ctx);
        let wide = screen.width() > 900.0;
        if self.view == 0 && self.config.show_inspector && wide {
            self.inspector(ctx);
        }
        let p = pal(ctx);
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(20, 14)),
            )
            .show(ctx, |ui| {
                self.banners(ui);
                match self.view {
                    0 => self.conversation_ui(ui),
                    1 => self.library_page(ui),
                    2 => self.connections_ui(ui),
                    3 => self.settings_ui(ui),
                    5 => self.models_page(ui),
                    6 => self.workbench_page(ui),
                    _ => self.characters_ui(ui),
                }
            });
        // After the pages and before the approval window, so the palette draws on top.
        self.quick_open(ctx);
        // Narrow windows: the context panel floats over the chat instead.
        if self.view == 0 && self.config.show_inspector && !wide {
            let mut open = true;
            egui::Window::new("Context")
                .open(&mut open)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 56.0))
                .default_width(280.0_f32.min(screen.width() - 40.0))
                .max_height((screen.height() - 90.0).max(160.0))
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| self.inspector_body(ui));
            if !open {
                self.config.show_inspector = false;
            }
        }
        self.apply_window(ctx, screen);
        let mut decision = None;
        if let Some(a) = &self.approval {
            let stale = self
                .approval_galley
                .as_ref()
                .is_none_or(|(title, len, _)| *title != a.title || *len != a.detail.len());
            if stale {
                let galley = mono_galley(ctx, &a.detail, p.text);
                self.approval_galley = Some((a.title.clone(), a.detail.len(), galley));
            }
            let galley = self.approval_galley.as_ref().map(|(_, _, g)| g.clone());
            egui::Window::new("Review proposed action")
                .collapsible(false)
                .resizable(true)
                .default_width(720.0_f32.min(screen.width() - 40.0))
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(RichText::new(&a.title).size(17.0).strong().color(p.accent));
                    let height = (screen.height() - 200.0).clamp(120.0, 480.0);
                    egui::ScrollArea::both().max_height(height).show(ui, |ui| {
                        if let Some(galley) = galley {
                            ui.add(egui::Label::new(galley).selectable(true));
                        }
                    });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Decline").clicked() {
                            decision = Some(false);
                        }
                        if Self::primary(ui, "Approve").clicked() {
                            decision = Some(true);
                        }
                    });
                });
        }
        if let Some(approved) = decision
            && let Some(a) = self.approval.take()
        {
            let _ = a.reply.send(approved);
            self.finish_apply();
        }
        let mut close_preview = false;
        if let Some((name, text)) = &self.preview {
            let stale = self
                .preview_galley
                .as_ref()
                .is_none_or(|(n, len, _)| n != name || *len != text.len());
            if stale {
                let galley = mono_galley(ctx, text, p.text);
                self.preview_galley = Some((name.clone(), text.len(), galley));
            }
            let galley = self.preview_galley.as_ref().map(|(_, _, g)| g.clone());
            let mut open = true;
            egui::Window::new(name)
                .open(&mut open)
                .default_width(760.0_f32.min(screen.width() - 40.0))
                .default_height(560.0_f32.min(screen.height() - 40.0))
                .constrain(true)
                .show(ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| {
                        if let Some(galley) = galley {
                            ui.add(egui::Label::new(galley).selectable(true));
                        }
                    });
                });
            if !open {
                close_preview = true;
            }
        }
        if close_preview {
            self.preview = None;
            self.preview_galley = None;
        }
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for session in &self.discord_sessions {
            session.disconnect();
        }
        if let Some(job) = &self.setup {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.llama.stop();
        self.embed_runtime.stop();
        self.workbench.shutdown();
        self.speaker.stop();
        crate::core::mcp::shutdown_all();
        self.cancel.store(true, Ordering::Relaxed);
        self.persist();
        // Let the writer finish every queued save before the process ends.
        self.writer = None;
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Studio {
    /// Event-driven redraws: worker threads wake the UI when something arrives, so only
    /// timers and in-progress downloads need a scheduled repaint.
    fn schedule_repaint(&self, ctx: &egui::Context, idle: bool) {
        let minimized = ctx.input(|i| i.viewport().minimized == Some(true));
        let tick = Duration::from_secs(3).saturating_sub(self.last_tick.elapsed());
        let wake = if minimized {
            Duration::from_secs(3)
        } else if self.setup.is_some() || self.update_progress.is_some() {
            Duration::from_millis(250)
        } else if !idle || self.speaking.is_some() {
            // Status text between events (each engine event already wakes the UI).
            tick.min(Duration::from_secs(1))
        } else if (self.view == 5 || self.metrics_dirty)
            && self.live_port.is_some()
            && self.llama.running()
        {
            // Wake for the next metrics read only when it can show something new.
            tick.min(
                self.metrics_every()
                    .saturating_sub(self.last_metrics.elapsed()),
            )
        } else {
            tick
        };
        ctx.request_repaint_after(wake.max(Duration::from_millis(30)));
    }
    /// "Apply to file": asks for the path, then opens the normal write review.
    fn apply_window(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let Some((path, _)) = &mut self.apply_draft else {
            return;
        };
        let (mut review, mut cancel, mut open) = (false, false, true);
        egui::Window::new("Apply to file")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0_f32.min(screen.width() - 40.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(path)
                        .hint_text("src/main.rs")
                        .desired_width(f32::INFINITY),
                );
                if !response.has_focus() && !response.lost_focus() {
                    response.request_focus();
                }
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.label(
                    RichText::new(format!("Relative to {}", self.config.workspace.display()))
                        .size(11.5)
                        .color(pal(ui.ctx()).muted),
                );
                ui.horizontal(|ui| {
                    review = Self::primary(ui, "Review").clicked() || enter;
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if cancel || !open {
            self.apply_draft = None;
        } else if review && let Some((path, code)) = self.apply_draft.take() {
            self.review_apply(path, code);
        }
    }
}

// ---- markdown --------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Inline {
    Plain,
    Code,
    Bold,
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
}
#[derive(Clone, Debug, PartialEq)]
enum Block {
    Heading(u8, String),
    Para(Vec<(String, Inline)>),
    /// Indent level, marker ("•" or "3."), text.
    Item(usize, String, Vec<(String, Inline)>),
    Quote(Vec<(String, Inline)>),
    Rule,
    Code {
        lang: String,
        code: String,
        spans: Vec<(usize, usize, Tok)>,
    },
    Table(Vec<Vec<String>>),
}

/// Splits `code spans` and **bold** out of a line of text.
fn inline_spans(text: &str) -> Vec<(String, Inline)> {
    let mut out: Vec<(String, Inline)> = vec![];
    let mut rest = text;
    let mut push = |s: &str, kind: Inline| {
        if s.is_empty() {
            return;
        }
        match out.last_mut() {
            Some((last, k)) if *k == kind => last.push_str(s),
            _ => out.push((s.to_owned(), kind)),
        }
    };
    while !rest.is_empty() {
        let code = rest.find('`');
        let bold = rest.find("**");
        match (code, bold) {
            (Some(c), b) if b.is_none_or(|b| c < b) => {
                push(&rest[..c], Inline::Plain);
                match rest[c + 1..].find('`') {
                    Some(end) => {
                        push(&rest[c + 1..c + 1 + end], Inline::Code);
                        rest = &rest[c + 2 + end..];
                    }
                    None => {
                        push(&rest[c..], Inline::Plain);
                        rest = "";
                    }
                }
            }
            (_, Some(b)) => {
                push(&rest[..b], Inline::Plain);
                match rest[b + 2..].find("**") {
                    Some(end) => {
                        push(&rest[b + 2..b + 2 + end], Inline::Bold);
                        rest = &rest[b + 4 + end..];
                    }
                    None => {
                        push(&rest[b..], Inline::Plain);
                        rest = "";
                    }
                }
            }
            _ => {
                push(rest, Inline::Plain);
                rest = "";
            }
        }
    }
    out
}

fn table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_owned()).collect()
}
fn is_table_rule(line: &str) -> bool {
    let t = line.trim();
    t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}
/// "- x", "* x", "+ x" or "12. x" → (marker, text).
fn list_item(t: &str) -> Option<(String, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(bullet) {
            return Some(("•".into(), rest));
        }
    }
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if (1..=3).contains(&digits) {
        let rest = &t[digits..];
        if let Some(text) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &t[..digits]), text));
        }
    }
    None
}

fn parse_markdown(text: &str) -> Vec<Block> {
    let mut blocks = vec![];
    let mut para = String::new();
    let flush = |para: &mut String, blocks: &mut Vec<Block>| {
        if !para.trim().is_empty() {
            blocks.push(Block::Para(inline_spans(para.trim_end())));
        }
        para.clear();
    };
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if let Some(lang) = t.strip_prefix("```") {
            flush(&mut para, &mut blocks);
            let mut code = String::new();
            for next in lines.by_ref() {
                if next.trim_start().starts_with("```") {
                    break;
                }
                code.push_str(next);
                code.push('\n');
            }
            code.pop();
            let lang = lang.trim().to_owned();
            let spans = highlight(&code, &lang);
            blocks.push(Block::Code { lang, code, spans });
        } else if t.trim().is_empty() {
            flush(&mut para, &mut blocks);
        } else if let Some(level) = t
            .find(|c| c != '#')
            .filter(|&n| (1..=6).contains(&n) && t[n..].starts_with(' '))
        {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Heading(level as u8, t[level..].trim().to_owned()));
        } else if t.trim_end().len() >= 3
            && ['-', '*', '_']
                .iter()
                .any(|&c| t.trim_end().chars().all(|x| x == c))
        {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Rule);
        } else if t.starts_with('|') {
            flush(&mut para, &mut blocks);
            let mut rows = vec![table_row(t)];
            while let Some(next) = lines.peek().filter(|l| l.trim_start().starts_with('|')) {
                if !is_table_rule(next) {
                    rows.push(table_row(next));
                }
                lines.next();
            }
            blocks.push(Block::Table(rows));
        } else if let Some((marker, rest)) = list_item(t) {
            flush(&mut para, &mut blocks);
            let indent = (line.len() - t.len()) / 2;
            blocks.push(Block::Item(indent.min(6), marker, inline_spans(rest)));
        } else if let Some(rest) = t.strip_prefix('>') {
            flush(&mut para, &mut blocks);
            blocks.push(Block::Quote(inline_spans(rest.trim())));
        } else {
            if !para.is_empty() {
                para.push('\n');
            }
            para.push_str(line);
        }
    }
    flush(&mut para, &mut blocks);
    blocks
}

/// Parses `text` into `blocks` from the stable point `(off, count)`: the first `count`
/// blocks are kept and only `text[off..]` is parsed. Returns the new stable point. Text
/// that ends in a blank line outside a code fence parses the same on its own (nothing in
/// `parse_markdown` carries state across such a line), so the result equals
/// `parse_markdown(text)`.
fn parse_from(text: &str, (off, count): (usize, usize), blocks: &mut Vec<Block>) -> (usize, usize) {
    blocks.truncate(count);
    let (mut off, mut count) = (off, count);
    let next = stable_point(text, off);
    if next > off {
        blocks.extend(parse_markdown(&text[off..next]));
        (off, count) = (next, blocks.len());
    }
    blocks.extend(parse_markdown(&text[off..]));
    (off, count)
}

/// The byte offset just past the last complete blank line outside a code fence, scanning
/// from `from` (0, or an earlier such offset); `from` itself when there is none. Fences
/// toggle on any line starting with ``` exactly as `parse_markdown` opens and closes them.
fn stable_point(text: &str, from: usize) -> usize {
    let mut fenced = false;
    let mut pos = from;
    let mut stable = from;
    for line in text[from..].split_inclusive('\n') {
        pos += line.len();
        if !line.ends_with('\n') {
            break;
        }
        let t = line.trim_start();
        if t.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && t.trim().is_empty() {
            stable = pos;
        }
    }
    stable
}

const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "use",
    "mod",
    "struct",
    "enum",
    "impl",
    "trait",
    "match",
    "if",
    "else",
    "for",
    "while",
    "loop",
    "return",
    "break",
    "continue",
    "in",
    "as",
    "where",
    "const",
    "static",
    "async",
    "await",
    "move",
    "self",
    "Self",
    "true",
    "false",
    "None",
    "Some",
    "Ok",
    "Err",
    "def",
    "class",
    "import",
    "from",
    "function",
    "var",
    "new",
    "null",
    "nil",
    "this",
    "try",
    "catch",
    "except",
    "finally",
    "with",
    "yield",
    "lambda",
    "int",
    "void",
    "char",
    "bool",
    "package",
    "func",
    "go",
    "defer",
    "interface",
    "type",
    "extends",
    "implements",
    "public",
    "private",
    "protected",
    "namespace",
    "using",
    "include",
    "then",
    "fi",
    "do",
    "done",
    "elif",
    "end",
    "local",
    "unsafe",
    "dyn",
    "ref",
    "crate",
    "super",
    "export",
    "default",
    "switch",
    "case",
    "not",
    "and",
    "or",
    "is",
    "pass",
    "raise",
    "True",
    "False",
    "undefined",
    "echo",
    "String",
    "Vec",
    "Option",
    "Result",
];

/// Byte spans covering `code`, tagged for simple highlighting.
fn highlight(code: &str, lang: &str) -> Vec<(usize, usize, Tok)> {
    let lang = lang.to_ascii_lowercase();
    let hash = matches!(
        lang.as_str(),
        "py" | "python"
            | "sh"
            | "bash"
            | "shell"
            | "zsh"
            | "toml"
            | "yaml"
            | "yml"
            | "rb"
            | "ruby"
            | "ps1"
            | "powershell"
            | "r"
            | "perl"
            | "ini"
            | "conf"
            | "dockerfile"
            | "makefile"
            | "cmake"
            | "nix"
    );
    let dash = matches!(lang.as_str(), "sql" | "lua" | "haskell" | "hs");
    let rust_like = matches!(
        lang.as_str(),
        "rust" | "rs" | "c" | "cpp" | "c++" | "java" | "cs"
    );
    let b = code.as_bytes();
    let mut spans: Vec<(usize, usize, Tok)> = vec![];
    let mut push = |s: usize, e: usize, t: Tok| {
        if s >= e {
            return;
        }
        match spans.last_mut() {
            Some(last) if last.2 == t && last.1 == s => last.1 = e,
            _ => spans.push((s, e, t)),
        }
    };
    let line_end = |i: usize| code[i..].find('\n').map_or(code.len(), |n| i + n);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let rest = &code[i..];
        if rest.starts_with("//") && !hash
            || (hash && c == b'#')
            || (dash && rest.starts_with("--"))
        {
            let end = line_end(i);
            push(i, end, Tok::Comment);
            i = end;
        } else if rest.starts_with("/*") && !hash {
            let end = rest[2..].find("*/").map_or(code.len(), |n| i + n + 4);
            push(i, end, Tok::Comment);
            i = end;
        } else if c == b'"' || c == b'\'' || c == b'`' {
            let limit = if c == b'\'' && rust_like {
                4
            } else {
                usize::MAX
            };
            let mut j = i + 1;
            let mut closed = None;
            while j < b.len() && b[j] != b'\n' && j - i <= limit {
                if b[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if b[j] == c {
                    closed = Some(j + 1);
                    break;
                }
                j += 1;
            }
            match closed {
                Some(end) => {
                    push(i, end, Tok::Str);
                    i = end;
                }
                None => {
                    push(i, i + 1, Tok::Plain);
                    i += 1;
                }
            }
        } else if c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80 {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                j += 1;
            }
            let word = &code[i..j];
            let tok = if c.is_ascii_digit() {
                Tok::Number
            } else if KEYWORDS.contains(&word) {
                Tok::Keyword
            } else {
                Tok::Plain
            };
            push(i, j, tok);
            i = j;
        } else {
            push(i, i + 1, Tok::Plain);
            i += 1;
        }
    }
    spans
}

fn inline_job(
    ui: &egui::Ui,
    spans: &[(String, Inline)],
    size: f32,
    color: Color32,
    p: &Palette,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let body = FontId::proportional(size);
    let strong = ui.visuals().strong_text_color();
    for (text, kind) in spans {
        let format = match kind {
            Inline::Plain => egui::TextFormat::simple(body.clone(), color),
            Inline::Bold => egui::TextFormat::simple(body.clone(), strong),
            Inline::Code => egui::TextFormat {
                font_id: FontId::monospace(size - 1.0),
                color,
                background: p.panel,
                ..Default::default()
            },
        };
        job.append(text, 0.0, format);
    }
    job
}

fn code_job(code: &str, spans: &[(usize, usize, Tok)], p: &Palette) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = FontId::monospace(13.0);
    for &(s, e, tok) in spans {
        let color = match tok {
            Tok::Plain => p.text,
            Tok::Keyword => p.keyword,
            Tok::Str => p.string,
            Tok::Comment => p.comment,
            Tok::Number => p.number,
        };
        job.append(
            &code[s..e],
            0.0,
            egui::TextFormat::simple(font.clone(), color),
        );
    }
    job
}

/// Renders parsed markdown; `id` keeps scroll areas of different messages apart. With
/// `apply` (Code mode, idle), code blocks offer "Apply", returned as (suggested path, code).
fn render_blocks(
    ui: &mut egui::Ui,
    blocks: &[Block],
    id: usize,
    apply: bool,
) -> Option<(String, String)> {
    let p = pal(ui.ctx());
    let mut applied = None;
    for (n, block) in blocks.iter().enumerate() {
        match block {
            Block::Heading(level, text) => {
                let size = match level {
                    1 => 18.0,
                    2 => 16.5,
                    _ => 15.0,
                };
                ui.add(egui::Label::new(RichText::new(text).strong().size(size)).wrap());
            }
            Block::Para(spans) => {
                ui.add(
                    egui::Label::new(inline_job(ui, spans, 14.0, p.text, &p))
                        .selectable(true)
                        .wrap(),
                );
            }
            Block::Item(indent, marker, spans) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(*indent as f32 * 14.0);
                    ui.label(RichText::new(marker).color(p.muted));
                    ui.add(
                        egui::Label::new(inline_job(ui, spans, 14.0, p.text, &p))
                            .selectable(true)
                            .wrap(),
                    );
                });
            }
            Block::Quote(spans) => {
                egui::Frame::new()
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .inner_margin(egui::Margin::symmetric(10, 4))
                    .corner_radius(4)
                    .show(ui, |ui| {
                        ui.visuals_mut().override_text_color = Some(p.muted);
                        ui.add(
                            egui::Label::new(inline_job(ui, spans, 14.0, p.muted, &p))
                                .selectable(true)
                                .wrap(),
                        );
                    });
            }
            Block::Rule => {
                ui.separator();
            }
            Block::Code { lang, code, spans } => {
                egui::Frame::new()
                    .fill(p.panel)
                    .corner_radius(8)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(lang).size(10.0).color(p.muted));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Copy").clicked() {
                                        ui.ctx().copy_text(code.clone());
                                    }
                                    if apply
                                        && ui
                                            .small_button("Apply")
                                            .on_hover_text(
                                                "Write this code to a workspace file, after review",
                                            )
                                            .clicked()
                                    {
                                        applied = Some((suggested_path(lang, code), code.clone()));
                                    }
                                },
                            );
                        });
                        egui::ScrollArea::horizontal()
                            .id_salt(("code", id, n))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(code_job(code, spans, &p))
                                        .selectable(true)
                                        .extend(),
                                );
                            });
                    });
            }
            Block::Table(rows) => {
                egui::ScrollArea::horizontal()
                    .id_salt(("table", id, n))
                    .show(ui, |ui| {
                        egui::Grid::new(("table_grid", id, n))
                            .striped(true)
                            .spacing([14.0, 6.0])
                            .show(ui, |ui| {
                                for (r, row) in rows.iter().enumerate() {
                                    for cell in row {
                                        let text = RichText::new(cell);
                                        ui.label(if r == 0 { text.strong() } else { text });
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rollback_removes_whole_turns() {
        let mut h = vec![
            json!({"role":"user","content":"a"}),
            json!({"role":"assistant","content":"1"}),
            json!({"role":"user","content":"b"}),
            json!({"role":"assistant","content":"2","tool_calls":[]}),
            json!({"role":"tool","content":"x"}),
            json!({"role":"assistant","content":"3"}),
        ];
        rollback_history(&mut h, 1);
        assert_eq!(h.len(), 2);
        rollback_history(&mut h, 5);
        assert!(h.is_empty());
    }
    #[test]
    fn group_round_keeps_one_user_turn() {
        let mut h = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"[A] hi"}),
        ];
        let returned = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"[A] hi"}),
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"[B] hello"}),
        ];
        append_group_reply(&mut h, returned);
        assert_eq!(h.len(), 3);
        assert_eq!(h.iter().filter(|m| m["role"] == "user").count(), 1);
        assert_eq!(h[2]["content"], "[B] hello");
    }
    #[test]
    fn titles_and_ids() {
        let messages = vec![Message::new("You", "\n  Fix the build  \nmore")];
        assert_eq!(title_from(&messages), "Fix the build");
        assert_eq!(title_from(&[]), "New chat");
        assert!(title_from(&[Message::new("You", "x".repeat(80))]).ends_with('…'));
        assert!(valid_id("18f3a2b"));
        assert!(!valid_id("../x") && !valid_id(""));
    }
    /// A long reply mixing every block kind, with blank lines inside code fences.
    fn sample_reply(target: usize) -> String {
        let part = "## Step\n\nSome **bold** text and `code` that\nwraps over two lines.\n\n\
            - first item\n  - nested item\n1. numbered\n\n> a quote\n\n| a | b |\n|---|---|\n\
            | 1 | 2 |\n\n```rust\nfn main() {\n    let s = \"hi\"; // note\n\n\
            \x20   println!(\"{s}\");\n}\n```\n---\n\n```\n\n```\nTail para\r\n\r\n";
        let mut text = String::new();
        while text.len() < target {
            text.push_str(part);
        }
        text
    }
    #[test]
    fn streaming_parse_matches_full_parse() {
        for text in [
            sample_reply(3000),
            "```py\nx = 1\n\ny = 2\n".into(),
            "para\n\n```\nopen\n\nfence never closed".into(),
            "  \n\n\n|x|\n\n|y|\n".into(),
        ] {
            let mut m = Message::new("AI", "");
            let mut at = 0;
            let mut step = 1;
            while at < text.len() {
                let mut end = (at + step).min(text.len());
                while !text.is_char_boundary(end) {
                    end += 1;
                }
                m.text.push_str(&text[at..end]);
                at = end;
                step = step % 13 + 1;
                assert_eq!(*m.blocks(), parse_markdown(&m.text), "at byte {at}");
            }
            // Freed and re-parsed from scratch gives the same blocks.
            m.free_blocks();
            assert_eq!(*m.blocks(), parse_markdown(&text));
        }
    }
    /// Times markdown parsing and highlighting of a 20 KB reply, whole and while streaming.
    #[test]
    fn perf_markdown_parse_20kb() {
        let text = sample_reply(20_000);
        let start = Instant::now();
        let blocks = parse_markdown(&text);
        let full = start.elapsed();
        let mut m = Message::new("AI", "");
        let start = Instant::now();
        for chunk in text.as_bytes().chunks(64) {
            m.text
                .push_str(std::str::from_utf8(chunk).expect("ascii sample"));
            m.blocks();
        }
        let streamed = start.elapsed();
        eprintln!(
            "perf_markdown_parse_20kb: {} blocks; full parse {full:?}; streamed in 64-byte \
             chunks ({} parses) {streamed:?}",
            blocks.len(),
            text.len().div_ceil(64)
        );
        assert_eq!(*m.blocks(), blocks);
        assert!(
            full < Duration::from_millis(500),
            "full parse took {full:?}"
        );
        assert!(
            streamed < Duration::from_secs(2),
            "streaming took {streamed:?}"
        );
    }
    #[test]
    fn perf_group_files_20k() {
        let files: Vec<String> = (0..20_000)
            .map(|i| format!("src/Mod{}/File{}.rs", i % 40, (i * 7919) % 20_000))
            .collect();
        let start = Instant::now();
        let groups = group_files(&files);
        let took = start.elapsed();
        eprintln!("perf_group_files_20k: {} folders in {took:?}", groups.len());
        assert_eq!(groups.len(), 40);
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }
    #[test]
    fn files_group_by_folder() {
        let files = vec![
            "src\\main.rs".to_string(),
            "Cargo.toml".into(),
            "src/ui/mod.rs".into(),
            "b.txt".into(),
        ];
        let groups = group_files(&files);
        assert_eq!(groups[0].0, "");
        assert_eq!(groups[0].1[0].0, "b.txt");
        assert_eq!(groups[1].0, "src");
        assert_eq!(groups[1].1[0].1, "src\\main.rs");
        assert_eq!(groups[2].0, "src/ui");
    }
    #[test]
    fn markdown_blocks() {
        let blocks = parse_markdown(
            "# Title\nSome `code` and **bold**\n\n- one\n  - two\n3. three\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nlet x = \"s\"; // c\n```\n---\n> quote",
        );
        assert_eq!(blocks[0], Block::Heading(1, "Title".into()));
        assert_eq!(
            blocks[1],
            Block::Para(vec![
                ("Some ".into(), Inline::Plain),
                ("code".into(), Inline::Code),
                (" and ".into(), Inline::Plain),
                ("bold".into(), Inline::Bold),
            ])
        );
        assert!(matches!(&blocks[2], Block::Item(0, m, _) if m == "•"));
        assert!(matches!(&blocks[3], Block::Item(1, _, _)));
        assert!(matches!(&blocks[4], Block::Item(0, m, _) if m == "3."));
        assert_eq!(
            blocks[5],
            Block::Table(vec![
                vec!["a".into(), "b".into()],
                vec!["1".into(), "2".into()]
            ])
        );
        assert!(
            matches!(&blocks[6], Block::Code { lang, code, .. } if lang == "rust" && code == "let x = \"s\"; // c")
        );
        assert_eq!(blocks[7], Block::Rule);
        assert!(matches!(&blocks[8], Block::Quote(_)));
        // An unclosed fence while streaming is still a code block.
        assert!(matches!(
            &parse_markdown("```py\nprint(1)")[0],
            Block::Code { .. }
        ));
        // Unclosed inline markers stay plain text.
        assert_eq!(inline_spans("a `b"), vec![("a `b".into(), Inline::Plain)]);
    }
    #[test]
    fn code_highlighting() {
        let code = "let s = \"hi\"; // note\nfn f<'a>() {}";
        let spans = highlight(code, "rust");
        let kind = |text: &str| {
            let start = code.find(text).unwrap();
            spans
                .iter()
                .find(|s| s.0 <= start && start < s.1)
                .unwrap()
                .2
        };
        assert_eq!(kind("let"), Tok::Keyword);
        assert_eq!(kind("\"hi\""), Tok::Str);
        assert_eq!(kind("// note"), Tok::Comment);
        assert_eq!(kind("'a"), Tok::Plain);
        // Spans cover the text exactly, on char boundaries.
        let unicode = "x = 'é' # ü\ny";
        let spans = highlight(unicode, "python");
        assert_eq!(spans.first().unwrap().0, 0);
        assert_eq!(spans.last().unwrap().1, unicode.len());
        assert!(spans.windows(2).all(|w| w[0].1 == w[1].0));
        assert!(
            spans
                .iter()
                .all(|s| unicode.is_char_boundary(s.0) && unicode.is_char_boundary(s.1))
        );
        assert_eq!(
            spans.iter().find(|s| s.2 == Tok::Comment).unwrap().1,
            unicode.find('\n').unwrap()
        );
    }
    #[test]
    fn fork_keeps_messages_and_matching_history() {
        let mut messages = vec![
            Message::new("You", "a"),
            Message::new("Assistant", "1"),
            Message::new("You", "b"),
            Message::new("Assistant", "2"),
        ];
        for m in &mut messages {
            m.in_history = true;
        }
        // At a reply: keep through it, drop the later turn.
        assert_eq!(fork_point(&messages, 1), (2, 1));
        // At a user message: stop before it (it becomes the draft).
        assert_eq!(fork_point(&messages, 2), (2, 1));
        assert_eq!(fork_point(&messages, 3), (4, 0));
        assert_eq!(fork_point(&messages, 0), (0, 2));
        // A turn that never reached the history is not rolled back.
        messages[2].in_history = false;
        assert_eq!(fork_point(&messages, 1), (2, 0));
        let mut h = vec![
            json!({"role":"user","content":"a"}),
            json!({"role":"assistant","content":"1"}),
            json!({"role":"user","content":"b"}),
            json!({"role":"assistant","content":"2"}),
        ];
        rollback_history(&mut h, fork_point(&messages[..2], 1).1);
        assert_eq!(h.len(), 4);
    }
    #[test]
    fn markdown_export_and_file_names() {
        let md = to_markdown(
            "Plan",
            &[Message::new("You", "hi\n"), Message::new("A", "yo")],
        );
        assert_eq!(md, "# Plan\n\n**You**\n\nhi\n\n**A**\n\nyo\n\n");
        assert_eq!(file_stem("Fix: the build?"), "Fix__the_build");
        assert_eq!(file_stem("???"), "chat");
    }
    #[test]
    fn apply_paths_come_from_info_string_or_first_comment() {
        assert_eq!(
            suggested_path("rust src/main.rs", "fn main() {}"),
            "src/main.rs"
        );
        assert_eq!(suggested_path("rust:src/lib.rs", ""), "src/lib.rs");
        assert_eq!(suggested_path("src/ui/mod.rs", ""), "src/ui/mod.rs");
        assert_eq!(
            suggested_path("python", "# file: app.py\nprint(1)"),
            "app.py"
        );
        assert_eq!(suggested_path("rust", "// src/a.rs\nfn a() {}"), "src/a.rs");
        assert_eq!(
            suggested_path("html", "<!-- index.html -->\n<p>"),
            "index.html"
        );
        assert_eq!(suggested_path("rust", "// a comment here\nfn a() {}"), "");
        assert_eq!(suggested_path("rust", "fn a() {}"), "");
        assert_eq!(suggested_path("", "// https://example.com/x.js"), "");
        assert_eq!(suggested_path("toml", "# .gitignore"), "");
    }
    #[test]
    fn attachments_by_extension_and_paste() {
        assert_eq!(image_mime(Path::new("a/B.PNG")), Some("image/png"));
        assert_eq!(image_mime(Path::new("x.jpeg")), Some("image/jpeg"));
        assert_eq!(image_mime(Path::new("x.gif")), None);
        assert_eq!(audio_mime(Path::new("v.wav")), Some("audio/wav"));
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot.webp");
        std::fs::write(&image, b"RIFF").unwrap();
        let text = format!("\"{}\"\n", image.display());
        assert_eq!(pasted_images(&text), Some(vec![image.clone()]));
        assert_eq!(pasted_images("just some text"), None);
        let missing = dir.path().join("gone.png");
        assert_eq!(pasted_images(&missing.display().to_string()), None);
        assert_eq!(pasted_images(""), None);
    }
    #[test]
    fn early_stop_notes_offer_continue() {
        assert!(stopped_early(
            "Reached the 12-step tool limit. Progress so far was kept; send a follow-up to continue."
        ));
        assert!(stopped_early(
            "Run ended early (Stopped). Progress so far was kept."
        ));
        assert!(!stopped_early("Tool: read_file"));
    }
    #[test]
    fn window_geometry_per_monitor() {
        assert_eq!(monitor_key(egui::vec2(1536.0, 864.0), 1.25), "1920x1080");
        let monitor = [1920.0, 1080.0];
        assert_eq!(
            restore_rect([100.0, 50.0, 1100.0, 720.0], monitor),
            Some([100.0, 50.0, 1100.0, 720.0])
        );
        // Off-screen, too big or nonsense values are not restored.
        assert_eq!(restore_rect([1500.0, 50.0, 1100.0, 720.0], monitor), None);
        assert_eq!(restore_rect([0.0, 0.0, 2500.0, 720.0], monitor), None);
        assert_eq!(restore_rect([f32::NAN, 0.0, 800.0, 600.0], monitor), None);
        assert_eq!(restore_rect([0.0, 0.0, 100.0, 100.0], monitor), None);
    }
    #[test]
    fn transcription_body_is_multipart() {
        let body = multipart_audio("B", "a\"b.wav", "audio/wav", b"RIFF", "m");
        let text = String::from_utf8(body).unwrap();
        assert!(
            text.starts_with("--B\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n")
        );
        assert!(text.contains(
            "name=\"file\"; filename=\"ab.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n--B--\r\n"
        ));
    }
    #[test]
    fn speech_skips_markup_and_code() {
        let text = speakable(
            "# Title\nSome **bold** `code`.\n```rust\nlet x = 1;\n```\n- item\n| a | b |",
        );
        assert_eq!(text, "Title\nSome bold code.\nitem\n");
    }
    #[test]
    fn conversation_index_round_trips_and_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let save = |id: &str, title: &str, updated: u64| {
            let c = Conversation {
                id: id.into(),
                title: title.into(),
                updated,
                messages: vec![Message::new("You", "x")],
                history: vec![],
            };
            config::save_json_compact(&dir.path().join(format!("{id}.json")), &c).unwrap();
        };
        save("a1", "First", 1);
        save("b2", "Second", 2);
        // No index yet: every file is parsed, newest first.
        let (list, rebuilt) = list_conversations(dir.path());
        assert!(rebuilt);
        assert_eq!(
            list.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["b2", "a1"]
        );
        config::save_json_compact(&index_path(dir.path()), &list).unwrap();
        let (indexed, rebuilt) = list_conversations(dir.path());
        assert!(!rebuilt);
        assert_eq!(indexed, list);
        // A file the index does not know about, or a damaged index, triggers a rescan.
        save("c3", "Third", 3);
        assert!(list_conversations(dir.path()).1);
        std::fs::write(index_path(dir.path()), b"{ broken").unwrap();
        let (list, rebuilt) = list_conversations(dir.path());
        assert!(rebuilt && list.len() == 3);
    }
    #[test]
    fn accent_colours() {
        assert_eq!(
            accent_shade([200, 100, 0], false),
            Color32::from_rgb(200, 100, 0)
        );
        assert_eq!(
            accent_shade([200, 100, 0], true),
            Color32::from_rgb(166, 83, 0)
        );
        assert_eq!(on_accent(Color32::from_rgb(250, 250, 200)), DARK.panel);
        assert_eq!(on_accent(Color32::from_rgb(30, 60, 160)), Color32::WHITE);
    }
}

#[cfg(test)]
mod ui_smoke {
    //! Headless UI smoke test (item 13): drives the real Studio through egui_kittest with no GPU or
    //! window, renders every page at a narrow and a wide size, and checks every button has a label.
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn ui_smoke_every_page() {
        let dir = std::env::temp_dir().join("kyttu-ui-smoke");
        let _ = std::fs::create_dir_all(&dir);
        for (width, collapsed) in [(1200.0, false), (700.0, true)] {
            let config = Config {
                workspace: dir.clone(),
                // No documentation sources, no first-run download, no update check.
                doc_sources: vec![],
                direct_backend: false,
                setup_attempted: true,
                auto_update: false,
                sidebar_collapsed: collapsed,
                ..Config::default()
            };
            let mut harness = Harness::builder()
                .with_size(egui::vec2(width, 760.0))
                .build_eframe(|cc| Studio::new(cc, config, None));
            // Out-of-range views fall through to the last page.
            for view in 0..8 {
                let app = harness.state_mut();
                app.view = view;
                // Keeps tick()'s 3 s autosave from writing these test settings over the real ones.
                app.last_tick = Instant::now();
                harness.run_steps(2);
                let unlabelled = harness
                    .query_all_by(|n| {
                        n.role() == egui::accesskit::Role::Button
                            && n.label().is_none_or(|l| l.trim().is_empty())
                    })
                    .count();
                assert_eq!(
                    unlabelled, 0,
                    "view {view} at {width} px: {unlabelled} buttons without a label"
                );
            }
            if !collapsed {
                assert!(harness.query_by_label("KYTTU").is_some());
            }
        }
    }
}

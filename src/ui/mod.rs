mod features;
mod views;
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
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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
fn pal(ctx: &egui::Context) -> Palette {
    if ctx.theme() == egui::Theme::Light {
        LIGHT
    } else {
        DARK
    }
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
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "C:/Windows/Fonts/segoeui.ttf",
            "C:/Windows/Fonts/seguiemj.ttf",
            "C:/Windows/Fonts/msyh.ttc",
        ]
    } else if cfg!(target_os = "macos") {
        // Apple Color Emoji is a bitmap font egui cannot draw.
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ]
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut added = false;
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        // egui panics on data that is not a font, so check the magic number.
        let magic: [u8; 4] = bytes
            .get(..4)
            .and_then(|m| m.try_into().ok())
            .unwrap_or_default();
        if bytes.len() > 64 << 20 || ![[0, 1, 0, 0], *b"OTTO", *b"ttcf", *b"true"].contains(&magic)
        {
            continue;
        }
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
        if self.cache.parsed_len != Some(self.text.len()) {
            self.cache.blocks = Arc::new(parse_markdown(&self.text));
            self.cache.parsed_len = Some(self.text.len());
            self.cache.size = Vec2::ZERO;
        }
        self.cache.blocks.clone()
    }
}
#[derive(Default, Serialize, Deserialize)]
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
#[derive(Clone, Default, Deserialize)]
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
    view: usize,
    coding: bool,
    use_docs: bool,
    pack: String,
    packs: Vec<String>,
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
    preview: Option<(String, String)>,
    search: String,
    search_results: Vec<Source>,
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
            items.sort_by_key(|(n, _)| n.to_lowercase());
            (dir, items)
        })
        .collect()
}
fn llama_log_tail() -> String {
    let text = std::fs::read_to_string(data_dir().join("llama-server.log")).unwrap_or_default();
    let lines: Vec<_> = text.lines().rev().take(12).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Loads the saved conversations, moving a pre-v3 conversation.json into the list.
fn load_conversations(config: &mut Config) -> (Vec<ConvMeta>, Conversation, Option<String>) {
    let dir = conversations_dir();
    let _ = std::fs::create_dir_all(&dir);
    let json_files = || {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect::<Vec<_>>()
    };
    let mut notice = None;
    let old = data_dir().join("conversation.json");
    if old.is_file() && json_files().is_empty() {
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
    let mut list: Vec<ConvMeta> = json_files()
        .into_iter()
        .filter_map(|path| {
            let id = path.file_stem()?.to_str()?.to_owned();
            valid_id(&id).then_some(())?;
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
        ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK, true));
        ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT, false));
        ctx.set_theme(theme_preference(&config.theme));
        ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = Vec2::new(8.0, 8.0);
            s.spacing.button_padding = Vec2::new(10.0, 5.0);
            s.spacing.interact_size.y = 26.0;
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
        let (conversations, conversation, conv_notice) = load_conversations(&mut config);
        let (tx, rx) = mpsc::channel();
        let bots = config.discord_bots.len();
        let mut app = Self {
            library,
            view: 0,
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
            preview: None,
            search: String::new(),
            search_results: vec![],
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
            config,
        };
        app.saved_config = serde_json::to_vec(&app.config).unwrap_or_default();
        app.refresh_files();
        app.refresh_packs();
        app.check();
        if app.config.docs.is_dir() && !app.library.matches_root(&app.config.docs) {
            app.library.rebuild(app.config.docs.clone());
        }
        if app.config.auto_update
            && now_secs().saturating_sub(app.config.last_update_check) > 6 * 3600
        {
            app.check_updates(false);
        }
        app
    }

    fn refresh_files(&mut self) {
        self.files = tools::list(&self.config.workspace)
            .lines()
            .map(str::to_owned)
            .collect();
        self.file_groups = group_files(&self.files);
    }
    fn refresh_packs(&mut self) {
        self.packs = std::fs::read_dir(&self.config.docs)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.'))
            .collect();
        self.packs.sort();
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
    fn save_config(&mut self) -> anyhow::Result<()> {
        self.sync_options();
        self.config.save()?;
        self.saved_config = serde_json::to_vec(&self.config).unwrap_or_default();
        Ok(())
    }
    /// Saves the open conversation (not while it is still empty).
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
        config::save_json(&path, c)?;
        let meta = ConvMeta {
            id: c.id.clone(),
            title: c.title.clone(),
            updated: c.updated,
        };
        self.config.active_conversation = meta.id.clone();
        self.conversations.retain(|m| m.id != meta.id);
        self.conversations.insert(0, meta);
        Ok(())
    }
    fn persist(&mut self) {
        let result = self.save_config().and_then(|_| self.save_conversation());
        if let Err(e) = result {
            self.error = Some(format!("Could not save: {e}"));
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
        let Some(path) = conversation_path(id) else {
            return;
        };
        let (loaded, note) = config::read_json::<Conversation>(&path);
        if note.is_some() {
            self.notice = note;
        }
        match loaded {
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
        let result = (|| -> anyhow::Result<()> {
            let (loaded, _) = config::read_json::<Conversation>(&path);
            let mut c = loaded.ok_or_else(|| anyhow::anyhow!("conversation could not be read"))?;
            c.title = title.clone();
            config::save_json(&path, &c)
        })();
        match result {
            Ok(()) => {
                if let Some(m) = self.conversations.iter_mut().find(|m| m.id == id) {
                    m.title = title;
                }
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
        let removed = if id == self.conversation.id {
            let c = std::mem::take(&mut self.conversation);
            self.config.active_conversation.clear();
            self.reset_chat_state();
            Some(c)
        } else {
            config::read_json::<Conversation>(&path).0
        };
        if let Some(c) = &removed {
            let _ = config::save_json(&data_dir().join("conversation.prev.json"), c);
        }
        let _ = std::fs::remove_file(&path);
        let mut bak = path.into_os_string();
        bak.push(".bak");
        let _ = std::fs::remove_file(bak);
        self.conversations.retain(|m| m.id != id);
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
        let tx = self.system_tx.clone();
        let config = self.config.clone();
        std::thread::spawn(move || {
            let _ = tx.send(SystemEvent::Models(
                crate::core::provider::models(&config).map_err(|e| e.to_string()),
            ));
            let _ = tx.send(SystemEvent::Embeddings(
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
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
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
            let _ = tx.send(SystemEvent::Started(result));
        });
    }
    /// On high-DPI displays (e.g. 175–200% scaling on a laptop) the default or
    /// remembered window size in points can exceed the screen, hiding the
    /// composer and status bar. Shrink and center the window once the monitor
    /// size is known.
    fn fit_to_monitor(&mut self, ctx: &egui::Context) {
        if self.fitted {
            return;
        }
        let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) else {
            return;
        };
        self.fitted = true;
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
        let (inner, outer, plain) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.inner_rect,
                v.outer_rect,
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
            self.config.window_pos = Some([(r.min.x * zoom).round(), (r.min.y * zoom).round()]);
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
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
            let result = runtime.start(&config).map_err(|e| format!("{e:#}"));
            let _ = tx.send(SystemEvent::Llama(result));
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
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
            let result = setup::auto_setup(&config, progress, cancel).map_err(|e| format!("{e:#}"));
            let _ = tx.send(SystemEvent::Setup(result));
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
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
            let result = update::check().map_err(|e| format!("{e:#}"));
            let _ = tx.send(SystemEvent::UpdateChecked(result, manual));
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
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
            let result =
                update::download_and_stage(&release, progress).map_err(|e| format!("{e:#}"));
            let _ = tx.send(SystemEvent::UpdateStaged(result));
        });
    }
    /// True when no reply, review or group round is in progress.
    fn idle_for_update(&self) -> bool {
        self.worker.is_none() && self.approval.is_none() && self.pending_characters.is_empty()
    }
    /// Swaps in a staged update and closes; on failure brings the runtime back up.
    fn apply_update(&mut self, path: PathBuf) {
        self.persist();
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
        self.server_found = llama::find_server(&self.config);
        if let Some(path) = self.server_found.clone()
            && !self.version_probe
            && self.server_version.as_ref().map(|(p, _)| p) != Some(&path)
        {
            self.version_probe = true;
            let tx = self.system_tx.clone();
            std::thread::spawn(move || {
                let version = llama::server_version(&path);
                let _ = tx.send(SystemEvent::ServerVersion(path, version));
            });
        }
        if !self.llama_busy && !self.crash_reported && self.llama.exited_unexpectedly() {
            self.crash_reported = true;
            self.llama_crash = Some(llama_log_tail());
            self.online = false;
            self.check();
        }
        // Debounced autosave: settings are written when they differ from disk.
        self.sync_options();
        if serde_json::to_vec(&self.config).is_ok_and(|b| b != self.saved_config)
            && let Err(e) = self.save_config()
        {
            self.error = Some(format!("Could not save settings: {e}"));
        }
    }
    fn poll(&mut self) {
        while let Ok(event) = self.system_rx.try_recv() {
            match event {
                SystemEvent::Llama(result) => {
                    self.llama_busy = false;
                    match result {
                        Ok(port) => {
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
                        }
                        Err(e) => {
                            self.status = e.clone();
                            self.error = Some(e);
                        }
                    }
                    self.refresh_files();
                    self.persist();
                }
            }
        }
        if self.worker.is_none() && !self.pending_characters.is_empty() {
            if self.cancel.load(Ordering::Relaxed) || self.error.is_some() {
                self.pending_characters.clear();
            } else if let Some(persona) = self.pending_characters.pop_front() {
                self.round_follow = true;
                self.start_character(self.group_question.clone(), persona);
            }
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
        if self.group_chat && !self.coding && !self.config.personas.iter().any(|p| p.in_group) {
            self.error = Some("Create a character and include it in group chats first.".into());
            return false;
        }
        self.error = None;
        self.editing = None;
        self.sources.clear();
        self.activity.clear();
        self.selected_message = None;
        // in_history stays false until the reply finishes successfully.
        self.conversation
            .messages
            .push(Message::new("You", question.clone()));
        self.cancel = Arc::new(AtomicBool::new(false));
        self.group_question = question.clone();
        self.round_follow = false;
        self.pending_characters = if self.group_chat && !self.coding {
            self.config
                .personas
                .iter()
                .filter(|p| p.in_group)
                .take(6)
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
        self.start_character(question, persona);
        true
    }
    /// Replaces the replies to the last user message with new ones.
    fn regenerate(&mut self) {
        if self.worker.is_some() {
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
        if self.worker.is_some() || text.is_empty() || index >= self.conversation.messages.len() {
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
    fn start_character(&mut self, question: String, persona: Persona) {
        self.conversation
            .messages
            .push(Message::new(persona.name.clone(), String::new()));
        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        let mut config = self.config.clone();
        if !persona.model.is_empty() {
            config.model = persona.model.clone();
        }
        engine::spawn(
            engine::Request {
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
            },
            tx,
        );
    }
    /// Starts an empty conversation; the previous one stays in the sidebar.
    fn new_chat(&mut self) {
        self.persist();
        self.conversation = Conversation::default();
        self.config.active_conversation.clear();
        self.reset_chat_state();
    }
    fn choose_workspace(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_directory(&self.config.workspace)
            .pick_folder()
        {
            self.new_chat();
            self.config.workspace = path;
            self.refresh_files();
            self.preview = None;
            self.persist();
        }
    }
    fn choose_docs(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_directory(&self.config.docs)
            .pick_folder()
        {
            self.config.docs = path;
            self.pack.clear();
            self.sources.clear();
            self.search_results.clear();
            self.refresh_packs();
            self.persist();
            self.library.rebuild(self.config.docs.clone());
        }
    }
    fn choose_model_file(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("GGUF model", &["gguf"])
            .pick_file()
        {
            self.config.gguf_model = path;
            self.persist();
        }
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
                    ui.spinner();
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
            ui.spinner();
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

impl eframe::App for Studio {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.tick();
        if self.close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.theme_applied != self.config.theme {
            self.theme_applied = self.config.theme.clone();
            ctx.set_theme(theme_preference(&self.config.theme));
        }
        self.config.ui_scale = ctx.zoom_factor();
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
        }
        ctx.request_repaint_after(Duration::from_millis(
            if !idle || self.setup.is_some() || self.update_progress.is_some() {
                35
            } else {
                250
            },
        ));
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
                    1 => self.library_ui(ui),
                    2 => self.connections_ui(ui),
                    3 => self.settings_ui(ui),
                    _ => self.characters_ui(ui),
                }
            });
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
        let mut decision = None;
        if let Some(a) = &self.approval {
            egui::Window::new("Review proposed action")
                .collapsible(false)
                .resizable(true)
                .default_width(720.0_f32.min(screen.width() - 40.0))
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(RichText::new(&a.title).size(17.0).strong().color(p.accent));
                    let height = (screen.height() - 200.0).clamp(120.0, 480.0);
                    egui::ScrollArea::both().max_height(height).show(ui, |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(&a.detail).monospace()).selectable(true),
                        );
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
        }
        let mut close_preview = false;
        if let Some((name, text)) = &self.preview {
            let mut open = true;
            egui::Window::new(name)
                .open(&mut open)
                .default_width(760.0_f32.min(screen.width() - 40.0))
                .default_height(560.0_f32.min(screen.height() - 40.0))
                .constrain(true)
                .show(ctx, |ui| {
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(text).monospace()).selectable(true));
                    });
                });
            if !open {
                close_preview = true;
            }
        }
        if close_preview {
            self.preview = None;
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
        self.cancel.store(true, Ordering::Relaxed);
        self.persist();
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

/// Renders parsed markdown; `id` keeps scroll areas of different messages apart.
fn render_blocks(ui: &mut egui::Ui, blocks: &[Block], id: usize) {
    let p = pal(ui.ctx());
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
}

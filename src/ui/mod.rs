mod features;
mod views;
use crate::core::personas::Persona;
use crate::{
    config::{Config, data_dir},
    docs::{Library, Source},
    engine::{self, Approval, Event},
    tools,
};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

const BG: Color32 = Color32::from_rgb(21, 14, 30);
const PANEL: Color32 = Color32::from_rgb(15, 9, 23);
const CARD: Color32 = Color32::from_rgb(34, 23, 46);
const TEXT: Color32 = Color32::from_rgb(227, 233, 240);
const MUTED: Color32 = Color32::from_rgb(140, 155, 172);
const ACCENT: Color32 = Color32::from_rgb(247, 87, 108);
const BORDER: Color32 = Color32::from_rgb(52, 34, 66);
const SELECTED: Color32 = Color32::from_rgb(58, 26, 50);
const USER_BUBBLE: Color32 = Color32::from_rgb(48, 24, 42);

#[derive(Clone, Default, Serialize, Deserialize)]
struct Message {
    role: String,
    text: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Conversation {
    messages: Vec<Message>,
    history: Vec<Value>,
}
enum SystemEvent {
    Models(Result<Vec<String>, String>),
    Started(Result<(), String>),
    Embeddings(Vec<String>),
    Llama(Result<(), String>),
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
    preview: Option<(String, String)>,
    search: String,
    search_results: Vec<Source>,
    error: Option<String>,
    embedding_models: Vec<String>,
    use_embeddings: bool,
    use_web: bool,
    active_persona: usize,
    group_chat: bool,
    pending_characters: std::collections::VecDeque<Persona>,
    group_question: String,
    discord: crate::integrations::discord::Discord,
    discord_token: String,
    discord_webhook: String,
    llama: crate::core::llama::Runtime,
    llama_busy: bool,
    fitted: bool,
    auto_started: bool,
}

impl Studio {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        // Text-field background; egui's default is near-black.
        visuals.extreme_bg_color = PANEL;
        visuals.window_fill = CARD;
        visuals.override_text_color = Some(TEXT);
        visuals.selection.bg_fill = Color32::from_rgb(108, 37, 68);
        visuals.widgets.inactive.bg_fill = CARD;
        visuals.widgets.inactive.weak_bg_fill = CARD;
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(59, 35, 71);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(59, 35, 71);
        visuals.widgets.active.bg_fill = Color32::from_rgb(88, 34, 64);
        visuals.widgets.active.weak_bg_fill = Color32::from_rgb(88, 34, 64);
        visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        cc.egui_ctx.set_visuals(visuals);
        cc.egui_ctx.style_mut(|s| {
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
        let config = Config::load();
        // Saved interface scale, applied on top of the OS display scaling.
        cc.egui_ctx.set_zoom_factor(if config.ui_scale.is_finite() {
            config.ui_scale.clamp(0.7, 1.8)
        } else {
            1.0
        });
        let _ = std::fs::create_dir_all(data_dir());
        let library = Library::new(data_dir().join("documentation.sqlite"));
        let conversation = std::fs::read(data_dir().join("conversation.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            config,
            library,
            view: 0,
            coding: true,
            use_docs: true,
            pack: String::new(),
            packs: vec![],
            draft: String::new(),
            conversation,
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
            preview: None,
            search: String::new(),
            search_results: vec![],
            error: None,
            embedding_models: vec![],
            use_embeddings: false,
            use_web: false,
            active_persona: 0,
            group_chat: false,
            pending_characters: Default::default(),
            group_question: String::new(),
            discord: Default::default(),
            discord_token: String::new(),
            discord_webhook: String::new(),
            llama: Default::default(),
            llama_busy: false,
            fitted: false,
            auto_started: false,
        };
        app.refresh_files();
        app.refresh_packs();
        app.check();
        if app.config.docs.is_dir() && !app.library.matches_root(&app.config.docs) {
            app.library.rebuild(app.config.docs.clone());
        }
        app
    }

    fn refresh_files(&mut self) {
        self.files = tools::list(&self.config.workspace)
            .lines()
            .map(str::to_owned)
            .collect();
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
    fn persist(&mut self) {
        let result = self.config.save().and_then(|_| {
            std::fs::write(
                data_dir().join("conversation.json"),
                serde_json::to_vec_pretty(&self.conversation)?,
            )?;
            Ok(())
        });
        if let Err(e) = result {
            self.error = Some(format!("Could not save settings: {e}"));
        }
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
    /// On high-DPI displays (e.g. 175–200% scaling on a laptop) the default
    /// window size in points can exceed the screen, hiding the composer and
    /// status bar. Shrink and center the window once the monitor size is known.
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
    fn start_llama(&mut self) {
        if self.llama_busy {
            return;
        }
        self.llama_busy = true;
        self.connection = "Loading model…".into();
        let runtime = self.llama.clone();
        let config = self.config.clone();
        let tx = self.system_tx.clone();
        std::thread::spawn(move || {
            let result = runtime.start(&config).map_err(|e| e.to_string());
            let _ = tx.send(SystemEvent::Llama(result));
        });
    }
    fn poll(&mut self) {
        while let Ok(event) = self.system_rx.try_recv() {
            if let SystemEvent::Llama(result) = event {
                self.llama_busy = false;
                match result {
                    Ok(()) => {
                        self.config.direct_backend = true;
                        self.config.direct_url =
                            format!("http://127.0.0.1:{}", self.config.llama_port);
                        self.config.model = crate::core::llama::model_alias(&self.config);
                        self.persist();
                    }
                    Err(e) => self.error = Some(e),
                }
                self.check();
                continue;
            }
            self.checking = false;
            match event {
                SystemEvent::Llama(_) => {}
                SystemEvent::Embeddings(models) => {
                    self.embedding_models = models;
                }
                SystemEvent::Models(result) => match result {
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
                        // llama.cpp is primary: load the saved model once at launch
                        // when no server is already listening.
                        if self.config.direct_backend
                            && !self.auto_started
                            && self.config.gguf_model.is_file()
                        {
                            self.auto_started = true;
                            self.start_llama();
                        } else if self.config.direct_backend && !self.config.gguf_model.is_file() {
                            self.status = "Choose a GGUF model in Settings to start.".into();
                        }
                    }
                },
                SystemEvent::Started(result) => {
                    if let Err(e) = result {
                        self.error = Some(e);
                    }
                    self.check();
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
                    for source in sources {
                        if !self
                            .sources
                            .iter()
                            .any(|s| s.path == source.path && s.line == source.line)
                        {
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
                            self.conversation.history = history;
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
                self.start_character(self.group_question.clone(), persona);
            }
        }
    }
    fn send(&mut self) {
        if self.worker.is_some() || self.draft.trim().is_empty() {
            return;
        }
        if self.group_chat && !self.coding && !self.config.personas.iter().any(|p| p.in_group) {
            self.error = Some("Create a character and include it in group chats first.".into());
            return;
        }
        let question: String = self.draft.trim().chars().take(6000).collect();
        self.draft.clear();
        self.error = None;
        self.sources.clear();
        self.activity.clear();
        self.conversation.messages.push(Message {
            role: "You".into(),
            text: question.clone(),
        });
        self.cancel = Arc::new(AtomicBool::new(false));
        self.group_question = question.clone();
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
    }
    fn start_character(&mut self, question: String, persona: Persona) {
        self.conversation.messages.push(Message {
            role: persona.name.clone(),
            text: String::new(),
        });
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
    fn new_chat(&mut self) {
        self.pending_characters.clear();
        self.conversation = Conversation::default();
        self.sources.clear();
        self.activity.clear();
        self.error = None;
        self.persist();
    }
    fn choose_workspace(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_directory(&self.config.workspace)
            .pick_folder()
        {
            self.config.workspace = path;
            self.new_chat();
            self.refresh_files();
            self.preview = None;
        }
    }
    fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(10)
            .inner_margin(14)
            .show(ui, body);
    }
    fn eyebrow(ui: &mut egui::Ui, text: &str) {
        ui.label(RichText::new(text).size(10.5).strong().color(MUTED));
    }
    fn page_title(ui: &mut egui::Ui, text: &str) {
        ui.heading(RichText::new(text).strong());
        ui.add_space(8.0);
    }
    fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
        ui.add(
            egui::Button::new(RichText::new(text).color(PANEL).strong())
                .fill(ACCENT)
                .corner_radius(8),
        )
    }
}

impl eframe::App for Studio {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        self.config.ui_scale = ctx.zoom_factor();
        self.fit_to_monitor(ctx);
        let screen = ctx.content_rect();
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::N)) && self.worker.is_none() {
            self.new_chat();
            self.view = 0;
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::O)) && self.worker.is_none() {
            self.choose_workspace();
        }
        ctx.request_repaint_after(Duration::from_millis(if self.worker.is_some() {
            35
        } else {
            250
        }));
        self.sidebar(ctx);
        // Hide the context panel when the window is too narrow to fit it.
        if self.view == 0 && self.config.show_inspector && screen.width() > 900.0 {
            self.inspector(ctx);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(20, 14)),
            )
            .show(ctx, |ui| {
                if let Some(error) = &self.error {
                    let mut dismiss = false;
                    egui::Frame::new()
                        .fill(Color32::from_rgb(70, 37, 40))
                        .corner_radius(8)
                        .inner_margin(10)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(error).color(Color32::from_rgb(255, 195, 190)),
                                );
                                if ui.small_button("Dismiss").clicked() {
                                    dismiss = true;
                                }
                            });
                        });
                    if dismiss {
                        self.error = None;
                    }
                    ui.add_space(12.0);
                }
                match self.view {
                    0 => self.conversation_ui(ui),
                    1 => self.library_ui(ui),
                    2 => self.connections_ui(ui),
                    3 => self.settings_ui(ui),
                    _ => self.characters_ui(ui),
                }
            });
        let mut decision = None;
        if let Some(a) = &self.approval {
            egui::Window::new("Review proposed action")
                .collapsible(false)
                .resizable(true)
                .default_width(720.0_f32.min(screen.width() - 40.0))
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.label(RichText::new(&a.title).size(17.0).strong().color(ACCENT));
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
        self.discord.disconnect();
        self.llama.stop();
        self.cancel.store(true, Ordering::Relaxed);
        self.persist();
    }
}

fn markdown(ui: &mut egui::Ui, text: &str, id: usize) {
    for (part, block) in text.split("```").enumerate() {
        if part % 2 == 1 {
            let (language, code) = block.split_once('\n').unwrap_or(("", block));
            egui::Frame::new()
                .fill(PANEL)
                .corner_radius(8)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(language).size(10.0).color(MUTED));
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(code.into());
                        }
                    });
                    egui::ScrollArea::horizontal()
                        .id_salt(("code", id, part))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(code).monospace()).selectable(true),
                            );
                        });
                });
        } else {
            for line in block.lines() {
                if line.starts_with('#') {
                    ui.label(
                        RichText::new(line.trim_start_matches('#').trim())
                            .strong()
                            .size(16.0),
                    );
                } else {
                    ui.add(egui::Label::new(line).selectable(true).wrap());
                }
            }
        }
    }
}

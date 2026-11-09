use super::*;

/// Sidebar navigation: icon, label and the view index it opens.
const NAV: [(&str, &str, usize); 7] = [
    (icons::CHAT, "Chat", 0),
    (icons::LIBRARY, "Library", 1),
    (icons::MODELS, "Models", 5),
    (icons::WORKBENCH, "Workbench", 6),
    (icons::CHARACTERS, "Characters", 4),
    (icons::CONNECTIONS, "Connections", 2),
    (icons::SETTINGS, "Settings", 3),
];

/// What a click in the message list asks for, applied after the list is drawn.
enum MessageAction {
    Regenerate,
    Edit(usize),
    Resend(usize),
    CancelEdit,
    Select(usize),
    Fork(usize),
    Speak(usize),
    Continue,
    Apply(String, String),
}
enum Fix {
    Model,
    Server,
    Start,
    Ollama,
    Workspace,
    Docs,
    Embeddings,
}
/// What a chat list menu asks for.
enum ChatAction {
    Open(String),
    Rename(String, String),
    Pin(String),
    Export(String),
    Delete(String),
}
/// One row of the sidebar file list.
enum FileRow<'a> {
    Dir(&'a str, bool),
    File(&'a str, &'a str, bool),
}

/// A frameless, muted text button used in message headers.
fn quiet_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = pal(ui.ctx());
    ui.add(egui::Button::new(RichText::new(text).size(11.0).color(p.muted)).frame(false))
}

/// Rough height of a message bubble that has not been laid out yet: header and margins plus
/// about 19 px per wrapped line. Only stands in for messages far from the viewport.
fn estimate_height(text: &str, width: f32) -> f32 {
    let per_line = ((width - 58.0) / 7.5).max(20.0);
    let rows: f32 = text
        .lines()
        .map(|l| (l.len() as f32 / per_line).ceil().max(1.0))
        .sum();
    44.0 + rows.max(1.0) * 19.0
}

/// "12.3 tok/s · 2.1k/8.2k ctx · 1.4 GB" for the composer's status line.
fn metrics_text(m: &llama::Metrics) -> String {
    let k = |n: usize| {
        if n >= 1000 {
            format!("{:.1}k", n as f64 / 1000.0)
        } else {
            n.to_string()
        }
    };
    let mut parts = vec![];
    if let Some(speed) = m.tokens_per_sec.filter(|s| s.is_finite() && *s > 0.0) {
        parts.push(format!("{speed:.1} tok/s"));
    }
    if m.ctx_total > 0 {
        parts.push(format!("{}/{} ctx", k(m.ctx_used), k(m.ctx_total)));
    }
    if m.ram_mb > 0 {
        parts.push(if m.ram_mb >= 1024 {
            format!("{:.1} GB", m.ram_mb as f64 / 1024.0)
        } else {
            format!("{} MB", m.ram_mb)
        });
    }
    parts.join(" · ")
}

/// Environment variable names typed one per line (or separated by commas or spaces).
fn parse_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    names.dedup();
    names
}

impl Studio {
    fn status_text(&self) -> &'static str {
        if !self.online {
            "Offline"
        } else if self.config.direct_backend {
            "llama.cpp"
        } else {
            "Ollama"
        }
    }
    /// The status dot, labelled for screen readers.
    fn status_dot(&self, ui: &mut egui::Ui, with_text: bool) {
        let text = self.status_text();
        let color = if self.online {
            pal(ui.ctx()).accent
        } else {
            Color32::LIGHT_RED
        };
        ui.horizontal(|ui| {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("Model server: {text}"),
                )
            });
            if with_text {
                ui.label(RichText::new(text).color(color).size(12.0));
            }
        })
        .response
        .on_hover_text(format!("{text} · {}", self.connection));
    }
    fn logo(ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 6.0, p.accent);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "K",
            FontId::proportional(16.0),
            p.on_accent,
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, "KYTTU - AI"));
    }

    pub(super) fn sidebar(&mut self, ctx: &egui::Context) {
        if self.config.sidebar_collapsed {
            self.rail(ctx);
            return;
        }
        let p = pal(ctx);
        egui::SidePanel::left("sidebar")
            .default_width(208.0)
            .width_range(170.0..=320.0)
            .resizable(true)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .inner_margin(egui::Margin::symmetric(12, 14)),
            )
            .show(ctx, |ui| {
                egui::TopBottomPanel::bottom("sidebar_footer")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6)))
                    .show_separator_line(false)
                    .show_inside(ui, |ui| {
                        ui.horizontal(|ui| {
                            self.status_dot(ui, true);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if quiet_button(ui, "«")
                                        .on_hover_text("Collapse sidebar (Ctrl+B)")
                                        .clicked()
                                    {
                                        self.config.sidebar_collapsed = true;
                                    }
                                },
                            );
                        });
                    });
                ui.horizontal(|ui| {
                    Self::logo(ui);
                    ui.label(RichText::new("KYTTU").size(16.0).strong());
                });
                ui.add_space(12.0);
                let width = ui.available_width();
                ui.add_enabled_ui(self.worker.is_none(), |ui| {
                    if ui
                        .add_sized(
                            [width, 30.0],
                            egui::Button::new(
                                RichText::new(icons::label(icons::NEW_CHAT, "New chat"))
                                    .color(p.on_accent)
                                    .strong(),
                            )
                            .fill(p.accent)
                            .corner_radius(8),
                        )
                        .on_hover_text("Ctrl+N")
                        .clicked()
                    {
                        self.new_chat();
                        self.view = 0;
                    }
                });
                ui.add_space(8.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                for (icon, label, view) in NAV {
                    let selected = self.view == view;
                    let button = egui::Button::new(
                        RichText::new(icons::label(icon, label)).color(if selected {
                            p.accent
                        } else {
                            p.text
                        }),
                    )
                    .fill(if selected { p.selected } else { p.panel })
                    .corner_radius(6);
                    let response = ui.add_sized([width, 28.0], button);
                    let response = if view == 6 {
                        response.on_hover_text("Editor, git and terminal · Ctrl+P opens a file")
                    } else {
                        response
                    };
                    if response.clicked() {
                        self.view = view;
                    }
                }
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.add_space(12.0);
                let rest = ui.available_height();
                self.chat_list(ui, width, (rest * 0.45).max(90.0));
                ui.add_space(8.0);
                self.project_files(ui);
            });
    }

    /// Ctrl+B: the sidebar shrunk to icons.
    fn rail(&mut self, ctx: &egui::Context) {
        let p = pal(ctx);
        egui::SidePanel::left("sidebar_rail")
            .exact_width(52.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .inner_margin(egui::Margin::symmetric(6, 14)),
            )
            .show(ctx, |ui| {
                egui::TopBottomPanel::bottom("rail_footer")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6)))
                    .show_separator_line(false)
                    .show_inside(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            if quiet_button(ui, "»")
                                .on_hover_text("Expand sidebar (Ctrl+B)")
                                .clicked()
                            {
                                self.config.sidebar_collapsed = false;
                            }
                            self.status_dot(ui, false);
                        });
                    });
                ui.vertical_centered(|ui| {
                    Self::logo(ui);
                    ui.add_space(10.0);
                    let new = ui.add_enabled(
                        self.worker.is_none(),
                        egui::Button::new(
                            RichText::new(icons::NEW_CHAT)
                                .color(p.on_accent)
                                .strong()
                                .size(16.0),
                        )
                        .fill(p.accent)
                        .corner_radius(8)
                        .min_size(Vec2::splat(32.0)),
                    );
                    if new.on_hover_text("New chat (Ctrl+N)").clicked() {
                        self.new_chat();
                        self.view = 0;
                    }
                    ui.add_space(6.0);
                    for (icon, label, view) in NAV {
                        let selected = self.view == view;
                        let button = egui::Button::new(RichText::new(icon).size(16.0))
                            .fill(if selected { p.selected } else { p.panel })
                            .corner_radius(6)
                            .min_size(Vec2::splat(34.0));
                        let response = ui.add(button).on_hover_text(label);
                        response.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label)
                        });
                        if response.clicked() {
                            self.view = view;
                        }
                    }
                });
            });
    }

    fn chat_list(&mut self, ui: &mut egui::Ui, width: f32, max_height: f32) {
        let p = pal(ui.ctx());
        Self::eyebrow(ui, "CHATS");
        if self.conversations.len() > 5 {
            ui.add(
                egui::TextEdit::singleline(&mut self.conversation_search)
                    .hint_text(icons::label(icons::SEARCH, "Search chats"))
                    .desired_width(width),
            );
        }
        let query = self.conversation_search.trim().to_lowercase();
        let idle = self.worker.is_none();
        let pins = &self.config.pinned_conversations;
        // Matching rows, pinned first (in pin order), then newest first.
        let mut rows: Vec<usize> = (0..self.conversations.len())
            .filter(|&i| {
                query.is_empty() || self.conversations[i].title.to_lowercase().contains(&query)
            })
            .collect();
        rows.sort_by_key(|&i| {
            pins.iter()
                .position(|p| *p == self.conversations[i].id)
                .unwrap_or(usize::MAX)
        });
        let pinned: std::collections::HashSet<&str> = pins.iter().map(String::as_str).collect();
        let (mut action, mut renamed) = (None, None);
        if self.conversations.is_empty() {
            ui.label(RichText::new("No saved chats").size(11.5).color(p.muted));
        }
        let row_height = ui.spacing().interact_size.y.max(24.0);
        egui::ScrollArea::vertical()
            .id_salt("chats")
            .max_height(max_height)
            .auto_shrink([false, true])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for &i in &rows[range] {
                    let meta = &self.conversations[i];
                    if let Some((id, text)) = &mut self.renaming
                        && *id == meta.id
                    {
                        let response =
                            ui.add(egui::TextEdit::singleline(text).desired_width(width));
                        if response.lost_focus() {
                            let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
                            renamed = Some((!cancelled).then(|| (id.clone(), text.clone())));
                        } else if !response.has_focus() {
                            response.request_focus();
                        }
                        continue;
                    }
                    let selected = meta.id == self.conversation.id;
                    let is_pinned = pinned.contains(meta.id.as_str());
                    ui.horizontal(|ui| {
                        let title = if is_pinned {
                            format!("{} {}", icons::PIN, meta.title)
                        } else {
                            meta.title.clone()
                        };
                        let button =
                            egui::Button::new(RichText::new(title).size(12.5).color(if selected {
                                p.accent
                            } else {
                                p.text
                            }))
                            .fill(if selected { p.selected } else { p.panel })
                            .corner_radius(6)
                            .truncate();
                        let response = ui
                            .add_enabled_ui(idle || selected, |ui| {
                                ui.add_sized([(width - 26.0).max(40.0), 24.0], button)
                            })
                            .inner
                            .on_hover_text(&meta.title);
                        if response.clicked() {
                            action = Some(ChatAction::Open(meta.id.clone()));
                        }
                        let more =
                            ui.menu_button(RichText::new(icons::MORE).color(p.muted), |ui| {
                                if ui.button(icons::label(icons::EDIT, "Rename")).clicked() {
                                    action = Some(ChatAction::Rename(
                                        meta.id.clone(),
                                        meta.title.clone(),
                                    ));
                                    ui.close();
                                }
                                if ui
                                    .button(icons::label(
                                        icons::PIN,
                                        if is_pinned { "Unpin" } else { "Pin" },
                                    ))
                                    .clicked()
                                {
                                    action = Some(ChatAction::Pin(meta.id.clone()));
                                    ui.close();
                                }
                                if ui
                                    .button(icons::label(icons::EXPORT, "Export as Markdown…"))
                                    .clicked()
                                {
                                    action = Some(ChatAction::Export(meta.id.clone()));
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(
                                        idle,
                                        egui::Button::new(icons::label(icons::TRASH, "Delete")),
                                    )
                                    .clicked()
                                {
                                    action = Some(ChatAction::Delete(meta.id.clone()));
                                    ui.close();
                                }
                            });
                        more.response.on_hover_text("Chat actions");
                    });
                }
            });
        if let Some(result) = renamed {
            self.renaming = None;
            if let Some((id, title)) = result {
                self.rename_conversation(&id, title);
            }
        }
        match action {
            Some(ChatAction::Open(id)) => self.open_conversation(&id),
            Some(ChatAction::Rename(id, title)) => self.renaming = Some((id, title)),
            Some(ChatAction::Pin(id)) => self.toggle_pin(&id),
            Some(ChatAction::Export(id)) => self.export_markdown(&id),
            Some(ChatAction::Delete(id)) => self.delete_conversation(&id),
            None => {}
        }
    }

    fn project_files(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let idle = self.worker.is_none();
        ui.horizontal(|ui| {
            Self::eyebrow(ui, "PROJECT");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(idle, |ui| {
                    if ui
                        .small_button(icons::OPEN_FOLDER)
                        .on_hover_text("Open a folder (Ctrl+O)")
                        .clicked()
                    {
                        self.choose_workspace();
                    }
                });
            });
        });
        // The project name opens the recent projects.
        let name = self
            .config
            .workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut switch = None;
        let mut clear = false;
        ui.menu_button(RichText::new(format!("{name} ▾")).strong(), |ui| {
            let recent: Vec<&PathBuf> = self
                .config
                .projects
                .iter()
                .filter(|path| **path != self.config.workspace)
                .collect();
            if recent.is_empty() {
                ui.label(
                    RichText::new("No recent projects")
                        .size(12.0)
                        .color(p.muted),
                );
            }
            for path in recent {
                let label = path.file_name().unwrap_or_default().to_string_lossy();
                let exists = path.is_dir();
                if ui
                    .add_enabled(idle && exists, egui::Button::new(&*label))
                    .on_hover_text(path.display().to_string())
                    .on_disabled_hover_text(if exists {
                        "Wait for the reply to finish."
                    } else {
                        "This folder no longer exists."
                    })
                    .clicked()
                {
                    switch = Some(path.clone());
                    ui.close();
                }
            }
            if !self.config.projects.is_empty() {
                ui.separator();
                if ui
                    .button(icons::label(icons::CLOSE, "Clear list"))
                    .clicked()
                {
                    clear = true;
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text(self.config.workspace.display().to_string());
        if let Some(path) = switch {
            self.open_workspace(path);
        }
        if clear {
            let old = std::mem::take(&mut self.config.projects);
            self.undo_toast(
                "Cleared recent projects",
                Box::new(move |studio: &mut Studio| studio.config.projects = old),
                Box::new(|| {}),
            );
        }
        // Flat rows (root files, then folders with their files when expanded), virtualized.
        let mut rows = vec![];
        for (dir, items) in &self.file_groups {
            let open = dir.is_empty() || self.open_dirs.contains(dir);
            if !dir.is_empty() {
                rows.push(FileRow::Dir(dir, open));
            }
            if open {
                rows.extend(
                    items
                        .iter()
                        .map(|(n, full)| FileRow::File(n, full, !dir.is_empty())),
                );
            }
        }
        let (mut clicked, mut toggle) = (None, None);
        if self.files.is_empty() {
            ui.label(RichText::new("No files").size(11.5).color(p.muted));
        }
        let row_height = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt("files")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for row in &rows[range] {
                    match *row {
                        FileRow::Dir(dir, open) => {
                            let text = format!("{} {dir}/", if open { "▾" } else { "▸" });
                            if ui
                                .add(
                                    egui::Button::new(RichText::new(text).size(11.5).color(p.text))
                                        .frame(false)
                                        .truncate(),
                                )
                                .clicked()
                            {
                                toggle = Some(dir.to_owned());
                            }
                        }
                        FileRow::File(name, full, nested) => {
                            ui.horizontal(|ui| {
                                if nested {
                                    ui.add_space(12.0);
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new(name).size(11.5).color(p.muted),
                                        )
                                        .frame(false)
                                        .truncate(),
                                    )
                                    .on_hover_text(full)
                                    .clicked()
                                {
                                    clicked = Some(full.to_owned());
                                }
                            });
                        }
                    }
                }
            });
        if let Some(dir) = toggle
            && !self.open_dirs.remove(&dir)
        {
            self.open_dirs.insert(dir);
        }
        if let Some(file) = clicked {
            let text = tools::read(&self.config.workspace, &file).unwrap_or_else(|e| e.to_string());
            self.preview = Some((file, text));
        }
    }

    pub(super) fn inspector(&mut self, ctx: &egui::Context) {
        let p = pal(ctx);
        egui::SidePanel::right("inspector")
            .default_width(260.0)
            .width_range(200.0..=440.0)
            .frame(
                egui::Frame::new()
                    .fill(p.panel)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .inner_margin(14),
            )
            .show(ctx, |ui| self.inspector_body(ui));
    }

    pub(super) fn inspector_body(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let mut clear = false;
        egui::ScrollArea::vertical()
            .id_salt("inspector_scroll")
            .show(ui, |ui| {
                let selected = self
                    .selected_message
                    .and_then(|i| self.conversation.messages.get(i));
                let sources = selected.map_or(&self.sources, |m| &m.sources);
                ui.horizontal(|ui| {
                    Self::eyebrow(ui, "SOURCES");
                    if let Some(m) = selected {
                        if !m.role.is_empty() {
                            ui.label(RichText::new(&m.role).size(10.5).color(p.accent));
                        }
                        clear = ui.small_button("Latest").clicked();
                    }
                });
                if sources.is_empty() {
                    ui.label(RichText::new("None yet").color(p.muted).size(12.0));
                }
                for (i, source) in sources.iter().enumerate() {
                    egui::CollapsingHeader::new(
                        RichText::new(format!("[{}] {}", i + 1, source.path)).size(12.0),
                    )
                    .id_salt((i, &source.path))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("line {}", source.line))
                                .size(10.5)
                                .color(p.muted),
                        );
                        if source.path.starts_with("https://") || source.path.starts_with("http://")
                        {
                            ui.hyperlink_to("Open", &source.path);
                        }
                        ui.label(RichText::new(&source.text).size(12.0));
                        if ui.small_button(icons::label(icons::COPY, "Copy")).clicked() {
                            ui.ctx().copy_text(source.text.clone());
                        }
                    });
                }
                ui.add_space(14.0);
                Self::eyebrow(ui, "ACTIVITY");
                if self.activity.is_empty() {
                    ui.label(RichText::new("None yet").color(p.muted).size(12.0));
                }
                for activity in &self.activity {
                    ui.label(RichText::new(activity).size(11.5).color(p.muted));
                }
            });
        if clear {
            self.selected_message = None;
        }
    }

    pub(super) fn conversation_ui(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let idle = self.worker.is_none();
        let mut entered_code = false;
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(idle, |ui| {
                ui.selectable_value(&mut self.coding, false, icons::label(icons::CHAT, "Chat"));
                // Chat replies no longer re-list the workspace, so catch up on entering Code.
                entered_code = ui
                    .selectable_value(&mut self.coding, true, icons::label(icons::EDITOR, "Code"))
                    .changed();
                ui.separator();
                egui::ComboBox::from_id_salt("model")
                    .selected_text(if self.config.model.is_empty() || self.models.is_empty() {
                        "No model loaded"
                    } else {
                        &self.config.model
                    })
                    .width(150.0)
                    .show_ui(ui, |ui| {
                        for model in &self.models {
                            ui.selectable_value(&mut self.config.model, model.clone(), model);
                        }
                    })
                    .response
                    .on_hover_text("Model");
                if !self.config.personas.is_empty() {
                    let name = if self.config.use_character {
                        self.config
                            .personas
                            .get(self.active_persona)
                            .map_or("No character", |p| p.name.as_str())
                    } else {
                        "No character"
                    };
                    egui::ComboBox::from_id_salt("character")
                        .selected_text(name)
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(!self.config.use_character, "No character")
                                .on_hover_text("Chat as the plain agent")
                                .clicked()
                            {
                                self.config.use_character = false;
                                // Group chat needs characters.
                                self.group_chat = false;
                            }
                            for (i, p) in self.config.personas.iter().enumerate() {
                                let on = self.config.use_character && self.active_persona == i;
                                if ui.selectable_label(on, &p.name).clicked() {
                                    self.active_persona = i;
                                    self.config.use_character = true;
                                }
                            }
                        })
                        .response
                        .on_hover_text("Character");
                }
                let options = ui.menu_button(icons::label(icons::SETTINGS, "Options"), |ui| {
                    let docs_toggled = ui
                        .checkbox(&mut self.use_docs, "Use documentation")
                        .changed();
                    let rerank_toggled = ui
                        .checkbox(&mut self.use_embeddings, "Embedding rerank")
                        .on_hover_text(
                            "Rerank document matches with the embedding model chosen in Settings.",
                        )
                        .changed();
                    if rerank_toggled {
                        // The user's choice stops the automatic switch-on.
                        self.config.rerank_user_set = true;
                    }
                    if (docs_toggled || rerank_toggled) && self.use_docs && self.use_embeddings {
                        crate::knowledge::embeddings::prewarm(&self.config);
                    }
                    ui.checkbox(&mut self.use_web, "Web search")
                        .on_hover_text("Sends search queries to DuckDuckGo.");
                    ui.add_enabled(
                        !self.coding
                            && self.config.use_character
                            && !self.config.personas.is_empty(),
                        egui::Checkbox::new(&mut self.group_chat, "Group chat"),
                    )
                    .on_disabled_hover_text(if self.coding {
                        "Group chat is for Chat mode."
                    } else {
                        "Choose a character first."
                    });
                    ui.separator();
                    egui::ComboBox::from_id_salt("pack")
                        .selected_text(if self.pack.is_empty() {
                            "All documentation"
                        } else {
                            &self.pack
                        })
                        .width(170.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.pack, String::new(), "All documentation");
                            for pack in &self.packs {
                                ui.selectable_value(&mut self.pack, pack.clone(), pack);
                            }
                        });
                });
                options
                    .response
                    .on_hover_text("Documentation, web search, group chat");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(
                        self.config.show_inspector,
                        icons::label(icons::EYE, "Context"),
                    )
                    .on_hover_text("Show sources and activity")
                    .clicked()
                {
                    self.config.show_inspector = !self.config.show_inspector;
                }
            });
        });
        if entered_code {
            self.refresh_files();
        }
        ui.add_space(4.0);

        // One enclosed chat box: messages on top, the composer docked at its bottom.
        egui::Frame::new()
            .fill(p.panel)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(12)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::TopBottomPanel::bottom("composer")
                    .frame(
                        egui::Frame::new()
                            .fill(p.card)
                            .corner_radius(egui::CornerRadius {
                                sw: 12,
                                se: 12,
                                ..Default::default()
                            })
                            .inner_margin(egui::Margin::symmetric(14, 10)),
                    )
                    .show_separator_line(true)
                    .show_inside(ui, |ui| self.composer(ui));
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(14, 12)))
                    .show_inside(ui, |ui| self.messages(ui));
            });
    }

    fn messages(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .id_salt("conversation")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                // Content y = screen y - origin; the viewport is in content coordinates.
                let origin = ui.max_rect().top();
                // A centered column, capped so lines stay readable on wide windows.
                let full = ui.available_width();
                let width = full.min(860.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(((full - width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        if self.conversation.messages.is_empty() {
                            self.empty_state(ui, viewport.height());
                        } else {
                            self.message_list(ui, width, viewport, origin);
                        }
                    });
                });
            });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, height: f32) {
        if !self.online {
            ui.add_space(((height - 330.0) / 2.0).max(16.0));
            self.setup_checklist(ui);
            return;
        }
        // Roughly vertically centered: the title + chips block is ~80pt tall.
        ui.add_space(((height - 80.0) / 2.0 - 24.0).max(16.0));
        let p = pal(ui.ctx());
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("What are we working on?").size(20.0).strong());
            ui.add_space(10.0);
            // Suggestions are data (assets/prompts/suggestions.json, or the user's copy); they
            // are re-read every few seconds rather than every frame.
            let stale = self.suggestions.0 != self.coding
                || self
                    .suggestions
                    .1
                    .is_none_or(|at| at.elapsed() > Duration::from_secs(5));
            if stale {
                let list = prompts::suggestions(self.coding)
                    .into_iter()
                    .map(|s| (s.title, s.prompt))
                    .collect();
                self.suggestions = (self.coding, Some(Instant::now()), list);
            }
            let chips = self.suggestions.2.clone();
            // Measure the chip row so it can be centered as a unit.
            let spacing = ui.spacing().item_spacing.x;
            let pad = ui.spacing().button_padding.x * 2.0;
            let row: f32 = chips
                .iter()
                .map(|(t, _)| {
                    ui.fonts_mut(|f| {
                        f.layout_no_wrap(
                            t.clone(),
                            egui::TextStyle::Button.resolve(ui.style()),
                            p.text,
                        )
                        .size()
                        .x
                    }) + pad
                })
                .sum::<f32>()
                + spacing * (chips.len().saturating_sub(1)) as f32;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - row) / 2.0).max(0.0));
                for (title, prompt) in &chips {
                    if ui
                        .add(
                            egui::Button::new(title.as_str())
                                .fill(p.card)
                                .corner_radius(8),
                        )
                        .on_hover_text(prompt.as_str())
                        .clicked()
                    {
                        self.draft.clone_from(prompt);
                    }
                }
            });
        });
    }

    /// First-run checklist shown in the empty chat while no model answers. File checks
    /// come from `paths_ok`, refreshed every few seconds, not from a stat per frame.
    fn setup_checklist(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let direct = self.config.direct_backend;
        let [model_ok, workspace_ok, docs_ok] = self.paths_ok;
        let server_ok = self.server_found.is_some();
        let working = self.llama_busy || self.setup.is_some();
        // (done, label, button, fix, button enabled, button icon, required)
        let mut rows: Vec<(bool, &str, &str, Fix, bool, &str, bool)> = vec![];
        if direct {
            rows.push((
                model_ok,
                "Model file",
                "Choose…",
                Fix::Model,
                !working,
                icons::OPEN_FOLDER,
                true,
            ));
            rows.push((
                server_ok,
                "llama-server found",
                "Choose…",
                Fix::Server,
                !working,
                icons::OPEN_FOLDER,
                true,
            ));
            rows.push((
                self.online || self.llama.running(),
                "Model running",
                "Start",
                Fix::Start,
                model_ok && server_ok && !working,
                icons::REFRESH,
                true,
            ));
        } else {
            rows.push((
                self.online,
                "Ollama running",
                "Start",
                Fix::Ollama,
                !self.checking,
                icons::REFRESH,
                true,
            ));
        }
        rows.push((
            workspace_ok,
            "Workspace",
            "Open…",
            Fix::Workspace,
            self.worker.is_none(),
            icons::OPEN_FOLDER,
            true,
        ));
        rows.push((
            docs_ok,
            "Documentation (optional)",
            "Add…",
            Fix::Docs,
            true,
            icons::LIBRARY,
            false,
        ));
        rows.push((
            self.embed_installed(),
            "Embeddings (optional)",
            "Models…",
            Fix::Embeddings,
            true,
            icons::MODELS,
            false,
        ));
        let mut fix = None;
        let mut auto = false;
        let width = ui.available_width().min(440.0);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(width);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Get started").size(20.0).strong());
                });
                ui.add_space(6.0);
                Self::card(ui, |ui| {
                    for (ok, label, button, action, enabled, icon, required) in rows {
                        ui.horizontal(|ui| {
                            // Fixed-width icon column so labels line up whatever the glyph.
                            ui.add_sized(
                                [16.0, ui.spacing().interact_size.y],
                                egui::Label::new(
                                    RichText::new(if ok {
                                        icons::CHECK
                                    } else if required {
                                        icons::WARNING
                                    } else {
                                        "–"
                                    })
                                    .color(if ok {
                                        p.ok
                                    } else {
                                        p.muted
                                    }),
                                ),
                            );
                            ui.label(label);
                            if !ok {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .add_enabled(
                                                enabled,
                                                egui::Button::new(icons::label(icon, button)),
                                            )
                                            .clicked()
                                        {
                                            fix = Some(action);
                                        }
                                    },
                                );
                            }
                        });
                    }
                    if self.setup.is_some() {
                        ui.separator();
                        self.setup_progress(ui);
                    } else if self.llama_busy {
                        ui.separator();
                        ui.horizontal(|ui| {
                            busy(ui);
                            ui.label(RichText::new("Loading model…").color(p.muted));
                        });
                    } else if direct && (!model_ok || !server_ok) {
                        ui.separator();
                        let model = self.setup_choice();
                        auto = Self::primary(
                            ui,
                            &icons::label(icons::DOWNLOAD, "Set up automatically"),
                        )
                        .on_hover_text(format!(
                            "Downloads llama.cpp {} and {} ({:.1} GB).",
                            setup::LLAMA_TAG,
                            model.name,
                            model.size as f64 / 1e9
                        ))
                        .clicked();
                    }
                });
            });
        });
        if auto {
            self.start_setup();
        }
        match fix {
            Some(Fix::Model) => self.choose_model_file(),
            Some(Fix::Server) => self.choose_server(),
            Some(Fix::Start) => self.start_llama(),
            Some(Fix::Ollama) => self.start_ollama(),
            Some(Fix::Workspace) => self.choose_workspace(),
            Some(Fix::Docs) => self.view = 1,
            Some(Fix::Embeddings) => self.view = 5,
            None => {}
        }
    }

    fn setup_choice(&self) -> &'static setup::ModelChoice {
        setup::MODELS
            .iter()
            .find(|m| m.file == self.config.setup_model)
            .unwrap_or(self.recommended)
    }

    fn choose_server(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            self.config.llama_server = path;
            self.rescan_server();
            self.persist();
        }
    }

    fn message_list(&mut self, ui: &mut egui::Ui, width: f32, viewport: egui::Rect, origin: f32) {
        let p = pal(ui.ctx());
        let count = self.conversation.messages.len();
        let idle = self.worker.is_none();
        let can_apply = idle && self.coding;
        let can_continue = idle && self.can_continue;
        let speaking = self.speaking;
        let mut action = None;
        for i in 0..count {
            let editing = matches!(&self.editing, Some((e, _)) if *e == i);
            let m = &mut self.conversation.messages[i];
            // Off-screen messages with a known size are skipped, keeping only their space.
            let y = ui.cursor().top() - origin;
            let size = m.cache.size;
            let measured = m.cache.measured_len == Some(m.text.len());
            let far = |h: f32| y + h < viewport.min.y - 400.0 || y > viewport.max.y + 400.0;
            if size.y > 0.0 && measured && !editing && far(size.y) {
                ui.allocate_exact_size(egui::vec2(size.x.min(width), size.y), egui::Sense::hover());
                ui.add_space(8.0);
                // Its parse is rebuilt when it scrolls back, so long chats stay small.
                m.free_blocks();
                continue;
            }
            // A just-opened chat has no sizes yet: messages far from view get an estimate
            // (not stored) instead of a full parse and layout on the first frame; each is
            // measured when it comes within reach of the viewport.
            if m.cache.measured_len.is_none() && !editing {
                let len = m.text.len();
                let estimate = match m.cache.estimate {
                    Some((l, w, h)) if l == len && w == width => h,
                    _ => {
                        let h = estimate_height(&m.text, width);
                        m.cache.estimate = Some((len, width, h));
                        h
                    }
                };
                if far(estimate) {
                    ui.allocate_exact_size(egui::vec2(width - 2.0, estimate), egui::Sense::hover());
                    ui.add_space(8.0);
                    continue;
                }
            }
            let blocks = m.blocks();
            let mine = m.role == "You";
            let last = i + 1 == count;
            let streaming = !idle && last;
            let selected = self.selected_message == Some(i);
            let response = egui::Frame::new()
                .fill(if mine { p.user_bubble } else { p.card })
                .stroke(Stroke::new(
                    1.0_f32,
                    if selected {
                        p.accent
                    } else {
                        Color32::TRANSPARENT
                    },
                ))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_min_width(width - 30.0);
                    ui.horizontal(|ui| {
                        // "No character" replies carry no name.
                        if !m.role.is_empty() {
                            ui.label(RichText::new(&m.role).size(12.0).strong().color(if mine {
                                p.accent
                            } else {
                                p.muted
                            }));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Rare actions live in the "…" menu.
                            let can_speak = !mine && !m.text.is_empty() && !streaming;
                            if idle || can_speak {
                                let more = ui.menu_button(
                                    RichText::new(icons::MORE).size(11.0).color(p.muted),
                                    |ui| {
                                        if can_speak {
                                            let label = if speaking == Some(i) {
                                                icons::label(icons::STOP, "Stop reading")
                                            } else {
                                                icons::label(icons::SPEAK, "Speak")
                                            };
                                            if ui
                                                .button(label)
                                                .on_hover_text("Read aloud with the system voice")
                                                .clicked()
                                            {
                                                action = Some(MessageAction::Speak(i));
                                                ui.close();
                                            }
                                        }
                                        if idle && ui
                                            .button(icons::label(icons::FORK, "Fork from here"))
                                            .on_hover_text(
                                                "Start a new chat with the messages up to this one",
                                            )
                                            .clicked()
                                        {
                                            action = Some(MessageAction::Fork(i));
                                            ui.close();
                                        }
                                    },
                                );
                                more.response.on_hover_text("More");
                            }
                            if !m.text.is_empty()
                                && quiet_button(ui, &icons::label(icons::COPY, "Copy")).clicked()
                            {
                                ui.ctx().copy_text(m.text.clone());
                            }
                            if idle
                                && mine
                                && !editing
                                && quiet_button(ui, &icons::label(icons::EDIT, "Edit")).clicked()
                            {
                                action = Some(MessageAction::Edit(i));
                            }
                            if idle
                                && !mine
                                && last
                                && quiet_button(ui, &icons::label(icons::REGENERATE, "Regenerate"))
                                    .clicked()
                            {
                                action = Some(MessageAction::Regenerate);
                            }
                            if can_continue
                                && !mine
                                && last
                                && quiet_button(ui, "Continue")
                                    .on_hover_text(
                                        "The run stopped early; resume where it left off",
                                    )
                                    .clicked()
                            {
                                action = Some(MessageAction::Continue);
                            }
                            if !m.sources.is_empty()
                                && quiet_button(ui, &format!("Sources ({})", m.sources.len()))
                                    .clicked()
                            {
                                action = Some(MessageAction::Select(i));
                            }
                        });
                    });
                    if !m.images.is_empty() {
                        ui.label(
                            RichText::new(icons::label(icons::ATTACH, &m.images.join(", ")))
                                .size(11.5)
                                .color(p.muted),
                        );
                    }
                    if let Some((_, text)) = self.editing.as_mut().filter(|_| editing) {
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if Self::primary(ui, &icons::label(icons::SEND, "Send")).clicked() {
                                action = Some(MessageAction::Resend(i));
                            }
                            if ui.button("Cancel").clicked() {
                                action = Some(MessageAction::CancelEdit);
                            }
                            ui.label(
                                RichText::new("Later messages are replaced.")
                                    .size(11.0)
                                    .color(p.muted),
                            );
                        });
                    } else if m.text.is_empty() && streaming {
                        ui.horizontal(|ui| {
                            busy(ui);
                            ui.label(RichText::new(&self.status).color(p.muted));
                        });
                    } else if let Some((path, code)) =
                        render_blocks(ui, &blocks, i, can_apply && !mine)
                    {
                        action = Some(MessageAction::Apply(path, code));
                    }
                })
                .response;
            m.cache.size = response.rect.size();
            m.cache.measured_len = Some(m.text.len());
            ui.add_space(8.0);
        }
        match action {
            Some(MessageAction::Regenerate) => self.regenerate(),
            Some(MessageAction::Edit(i)) => {
                self.editing = Some((i, self.conversation.messages[i].text.clone()));
            }
            Some(MessageAction::Resend(i)) => {
                if let Some((_, text)) = self.editing.take() {
                    self.resend_edited(i, text);
                }
            }
            Some(MessageAction::CancelEdit) => self.editing = None,
            Some(MessageAction::Select(i)) => {
                self.selected_message = (self.selected_message != Some(i)).then_some(i);
                self.config.show_inspector = true;
            }
            Some(MessageAction::Fork(i)) => self.fork_conversation(i),
            Some(MessageAction::Speak(i)) => self.speak(i),
            Some(MessageAction::Continue) => self.continue_run(),
            Some(MessageAction::Apply(path, code)) => self.apply_draft = Some((path, code)),
            None => {}
        }
    }

    /// Why Send is disabled, if it is.
    fn send_blocked(&self) -> Option<&'static str> {
        if !self.online {
            Some(if self.setup.is_some() {
                "Setup is still downloading the model."
            } else if self.llama_busy {
                "The model is still loading."
            } else {
                "No model is running. Start one in Settings, or open a new chat for the setup steps."
            })
        } else if self.draft.trim().is_empty() {
            Some("Type a message first.")
        } else if !self.attachments.is_empty() && self.images_blocked() {
            // Attached before switching to a model without vision.
            Some(NO_VISION)
        } else {
            None
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let id = egui::Id::new("draft_input");
        let idle = self.worker.is_none();
        let focused = ui.memory(|m| m.has_focus(id));
        // Pasted image file paths become attachments instead of text.
        let pasted: Vec<PathBuf> = if focused {
            ui.input_mut(|i| {
                let mut found = vec![];
                i.events.retain(|e| match e {
                    egui::Event::Paste(text) => match pasted_images(text) {
                        Some(paths) => {
                            found.extend(paths);
                            false
                        }
                        None => true,
                    },
                    _ => true,
                });
                found
            })
        } else {
            vec![]
        };
        if !pasted.is_empty() {
            self.attach_paths(pasted);
        }
        // Enter sends and Shift+Enter adds a line: take Enter before the text box sees it.
        let submit = idle
            && self.online
            && focused
            && ui.input_mut(|i| {
                (!i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                    || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
            });
        if !self.attachments.is_empty() || self.transcribing {
            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                for (n, a) in self.attachments.iter().enumerate() {
                    egui::Frame::new()
                        .fill(p.panel)
                        .corner_radius(6)
                        .inner_margin(egui::Margin::symmetric(6, 2))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(icons::label(icons::ATTACH, &a.name)).size(11.5),
                                )
                                .on_hover_text(format!("{} KB", a.bytes.len() / 1024));
                                if quiet_button(ui, icons::CLOSE)
                                    .on_hover_text("Remove")
                                    .clicked()
                                {
                                    remove = Some(n);
                                }
                            });
                        });
                }
                if self.transcribing {
                    busy(ui);
                    ui.label(RichText::new("Transcribing…").size(11.5).color(p.muted));
                }
            });
            if let Some(n) = remove {
                self.attachments.remove(n);
            }
        }
        egui::ScrollArea::vertical()
            .id_salt("draft")
            .max_height(160.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.draft)
                        .id(id)
                        .hint_text("Message  ·  Enter to send, Shift+Enter for a new line")
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .frame(false),
                );
            });
        let mut send = submit;
        let (mut stop, mut attach) = (false, false);
        ui.horizontal(|ui| {
            let no_vision = self.images_blocked();
            attach = ui
                .add_enabled(idle && !no_vision, egui::Button::new(icons::ATTACH).frame(false))
                .on_hover_text(
                    "Attach images (PNG, JPEG, WebP) for a vision model, or audio to transcribe into the message. Images can also be dropped or pasted as files.",
                )
                .on_disabled_hover_text(if no_vision { NO_VISION } else { "Wait for the reply." })
                .clicked();
            // The empty reply bubble already shows the status while it waits.
            let waiting = !idle
                && self
                    .conversation
                    .messages
                    .last()
                    .is_some_and(|m| m.text.is_empty());
            if !waiting {
                ui.add(
                    egui::Label::new(RichText::new(&self.status).size(11.0).color(p.muted))
                        .truncate(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !idle {
                    stop = ui
                        .button(icons::label(icons::STOP, "Stop"))
                        .on_hover_text("Esc")
                        .clicked();
                } else {
                    let blocked = self.send_blocked();
                    let button =
                        egui::Button::new(
                            RichText::new(icons::label(icons::SEND, "Send"))
                                .color(p.on_accent)
                                .strong(),
                        )
                            .fill(p.accent)
                            .corner_radius(8);
                    send |= ui
                        .add_enabled(blocked.is_none(), button)
                        .on_disabled_hover_text(blocked.unwrap_or_default())
                        .clicked();
                }
                if let Some(m) = &self.metrics {
                    let text = metrics_text(m);
                    if !text.is_empty() {
                        ui.label(RichText::new(text).size(11.0).color(p.muted))
                            .on_hover_text("Generation speed · context in use · llama-server memory");
                    }
                }
            });
        });
        if attach {
            self.choose_attachments();
        }
        if stop {
            self.stop();
        } else if send {
            self.send();
        }
    }

    pub(super) fn settings_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Settings");
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(760.0));
                self.settings_content(ui)
            });
    }

    fn settings_content(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let locked = self.worker.is_some()
            || self.checking
            || self.llama_busy
            || self.setup.is_some()
            || self.any_bot_running();
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::MODELS, "MODEL PROVIDER"));
            ui.add_enabled_ui(!locked, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.config.direct_backend, true, "llama.cpp");
                    ui.selectable_value(&mut self.config.direct_backend, false, "Ollama")
                        .on_hover_text("Optional. Can also serve embeddings.");
                });
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&self.connection).strong());
                ui.label(
                    RichText::new(if self.config.direct_backend {
                        self.config.direct_url.as_str()
                    } else {
                        engine::OLLAMA
                    })
                    .monospace()
                    .color(p.muted),
                );
                if self.checking {
                    busy(ui);
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.checking,
                        egui::Button::new(icons::label(icons::REFRESH, "Check connection")),
                    )
                    .clicked()
                {
                    self.check();
                }
                if !self.config.direct_backend
                    && ui
                        .add_enabled(
                            !self.checking && !self.online,
                            egui::Button::new("Start Ollama"),
                        )
                        .clicked()
                {
                    self.start_ollama();
                }
            });
            if self.models.is_empty() {
                ui.label(
                    RichText::new(if self.config.direct_backend {
                        "No model loaded. Choose a GGUF file below and start it."
                    } else {
                        "No models found. Pull one in Ollama, e.g. qwen2.5-coder:3b."
                    })
                    .size(12.0)
                    .color(p.muted),
                );
            } else {
                ui.label(
                    RichText::new(self.models.join(" · "))
                        .size(12.0)
                        .color(p.muted),
                );
            }
            if self.config.direct_backend {
                ui.add_space(6.0);
                ui.separator();
                self.llama_settings(ui, locked);
                ui.add_space(6.0);
                ui.separator();
                self.setup_settings(ui, locked);
            }
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::SETTINGS, "GENERATION"));
            ui.add_enabled_ui(self.worker.is_none(), |ui| {
                ui.add(
                    egui::Slider::new(&mut self.config.context_tokens, 1024..=32768)
                        .text("Context tokens"),
                )
                .on_hover_text("Older CPUs: start with 2048. For llama.cpp this applies when the server starts.");
                ui.add(
                    egui::Slider::new(&mut self.config.response_tokens, 128..=2048)
                        .text("Response tokens"),
                )
                .on_hover_text("Older CPUs: start with 256.");
                ui.add(egui::Slider::new(&mut self.config.max_steps, 1..=40).text("Tool steps"))
                    .on_hover_text("Most tool calls a Code reply may make before it stops.");
                ui.add(
                    egui::Slider::new(&mut self.config.temperature, 0.0..=1.5)
                        .step_by(0.05)
                        .text("Temperature"),
                );
                ui.add(
                    egui::Slider::new(&mut self.config.top_p, 0.05..=1.0)
                        .step_by(0.05)
                        .text("Top-p"),
                );
                ui.add(
                    egui::Slider::new(&mut self.config.repeat_penalty, 1.0..=1.5)
                        .step_by(0.01)
                        .text("Repeat penalty"),
                );
            });
            self.embedding_settings(ui);
        });
        ui.add_space(12.0);
        self.agent_settings(ui);
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::EYE, "INTERFACE"));
            ui.horizontal(|ui| {
                ui.label("Theme");
                for (value, label) in [("dark", "Dark"), ("light", "Light"), ("system", "System")] {
                    if ui
                        .selectable_label(self.config.theme == value, label)
                        .clicked()
                    {
                        self.config.theme = value.into();
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("Accent");
                ui.color_edit_button_srgb(&mut self.config.accent);
                if self.config.accent != config::DEFAULT_ACCENT
                    && ui.small_button("Reset").clicked()
                {
                    self.config.accent = config::DEFAULT_ACCENT;
                }
            });
            let mut scale = ui.ctx().zoom_factor();
            if ui
                .add(
                    egui::Slider::new(&mut scale, 0.7..=1.8)
                        .step_by(0.05)
                        .text("Scale"),
                )
                .on_hover_text("Ctrl + / Ctrl - / Ctrl 0")
                .changed()
            {
                ui.ctx().set_zoom_factor(scale);
            }
            ui.checkbox(&mut self.config.compact, "Compact density")
                .on_hover_text("Tighter spacing and smaller buttons");
            ui.checkbox(
                &mut self.config.show_inspector,
                "Show context panel in Chat",
            );
            ui.checkbox(&mut self.config.sidebar_collapsed, "Collapse sidebar")
                .on_hover_text("Ctrl+B");
            ui.checkbox(&mut self.config.tts, "Read replies aloud")
                .on_hover_text("Uses the system voice. Each reply also has a Speak button.");
        });
        ui.add_space(12.0);
        self.privacy_settings(ui);
        ui.add_space(12.0);
        self.mcp_settings(ui);
        ui.add_space(12.0);
        self.updates_card(ui);
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::OPEN_FOLDER, "PROJECT & DATA"));
            ui.horizontal_wrapped(|ui| {
                ui.label(self.config.workspace.display().to_string())
                    .on_hover_text("File tools are limited to this folder. Approved commands run as your account.");
                if ui
                    .add_enabled(
                        self.worker.is_none(),
                        egui::Button::new(icons::label(icons::OPEN_FOLDER, "Change…")),
                    )
                    .clicked()
                {
                    self.choose_workspace();
                }
            });
            self.workspace_env_settings(ui);
            ui.label(
                RichText::new(data_dir().display().to_string())
                    .size(11.5)
                    .color(p.muted),
            )
            .on_hover_text("Settings, conversations, search index and file backups.");
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(icons::label(icons::SAVE, "Save settings"))
                    .clicked()
                {
                    self.persist();
                    self.toast("Settings saved");
                }
                if ui
                    .button(icons::label(icons::EXPORT, "Export conversation…"))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_file_name("conversation.json")
                        .save_file()
                {
                    match serde_json::to_vec_pretty(&self.conversation)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| {
                            std::fs::write(path, bytes)?;
                            Ok(())
                        }) {
                        Ok(()) => self.toast("Conversation exported"),
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if ui
                    .add_enabled(
                        !self.conversation.messages.is_empty(),
                        egui::Button::new(icons::label(icons::EXPORT, "Export as Markdown…")),
                    )
                    .clicked()
                {
                    let id = self.conversation.id.clone();
                    self.export_markdown(&id);
                }
                if ui
                    .button(icons::label(icons::COPY, "Copy diagnostics"))
                    .on_hover_text(
                        "Versions, paths and recent log lines, for a bug report. Nothing is sent.",
                    )
                    .clicked()
                {
                    ui.ctx().copy_text(diagnostics::report());
                    self.toast("Diagnostics copied");
                }
                if ui
                    .button(icons::label(icons::OPEN_FOLDER, "Open logs"))
                    .on_hover_text("crash.log and llama-server logs")
                    .clicked()
                    && let Err(e) = diagnostics::open_logs_folder()
                {
                    self.error = Some(format!("Could not open the logs folder: {e}"));
                }
            });
        });
    }

    /// Embedding backend for "Embedding rerank": a second llama-server, or Ollama.
    fn embedding_settings(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        ui.horizontal(|ui| {
            ui.label("Embeddings");
            for (value, label) in [("llama", "llama.cpp"), ("ollama", "Ollama")] {
                if ui
                    .selectable_label(self.config.embedding_backend == value, label)
                    .clicked()
                    && self.config.embedding_backend != value
                {
                    self.config.embedding_backend = value.into();
                    // The Models page and the rerank read the same setting; a llama.cpp
                    // embedding server is not needed while Ollama embeds.
                    if value == "ollama" {
                        self.embed_runtime.stop();
                    }
                }
            }
        });
        let mut open_models = false;
        if self.config.embedding_backend == "ollama" {
            let installed = self.embed_installed();
            ui.horizontal(|ui| {
                ui.label("Embedding model");
                egui::ComboBox::from_id_salt("embedding_model")
                    .selected_text(&self.config.embedding_model)
                    .show_ui(ui, |ui| {
                        for model in &self.embedding_models {
                            ui.selectable_value(
                                &mut self.config.embedding_model,
                                model.clone(),
                                model,
                            );
                        }
                    })
                    .response
                    .on_hover_text("Used by Embedding rerank in Chat options. Requires Ollama.");
                if !installed {
                    ui.label(RichText::new("Not installed").size(12.0).color(p.muted))
                        .on_hover_text("Install it from the Models page.");
                    open_models = ui
                        .small_button("Models…")
                        .on_hover_text("Install an embedding model with Ollama")
                        .clicked();
                }
            });
        } else {
            // What the embedding server serves: the chosen GGUF, else the downloaded default
            // (the same file the Models page marks active).
            let configured = &self.config.embedding_gguf;
            let (name, hover) = match &self.embed_path {
                Some(path) => (
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path.display().to_string(),
                ),
                None if !configured.as_os_str().is_empty() => (
                    "Embedding model missing".into(),
                    format!("{} was not found", configured.display()),
                ),
                None => (
                    "No embedding model".into(),
                    "Download one on the Models page".into(),
                ),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(name).size(12.0).color(p.muted))
                    .on_hover_text(hover);
                open_models = ui
                    .small_button("Models…")
                    .on_hover_text("Choose, download and start the embedding model")
                    .clicked();
            });
        }
        if open_models {
            self.view = 5;
        }
    }

    /// Web query review and trusted LAN endpoints.
    fn privacy_settings(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::CONNECTIONS, "NETWORK & PRIVACY"));
            ui.checkbox(&mut self.config.confirm_web_query, "Review web searches before sending")
                .on_hover_text("With Web search on, the automatic first query is shown for approval before it goes to DuckDuckGo.");
            let locked = self.worker.is_some() || self.llama.running();
            ui.add_enabled_ui(!locked, |ui| {
                ui.checkbox(&mut self.config.allow_lan, "Allow trusted computers on the network")
                    .on_hover_text("Lets the external server URL point at another computer, for example a stronger PC running llama-server.")
                    .on_disabled_hover_text("Stop the model and wait for the reply to finish.");
            });
            if self.config.allow_lan {
                Self::banner(ui, true, |ui| {
                    ui.label(
                        "Prompts, files and tool results are sent to these computers. Only add machines you control; use https outside your home network.",
                    );
                });
                let response = ui
                    .add(
                        egui::TextEdit::multiline(&mut self.trusted_draft)
                            .hint_text("192.168.1.20\nstudio.local")
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text("Host names or IP addresses, one per line, without http://.");
                if response.changed() {
                    self.config.trusted_endpoints = self
                        .trusted_draft
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                if self.config.trusted_endpoints.is_empty() {
                    ui.label(
                        RichText::new("No trusted hosts yet; only this computer is used.")
                            .size(11.5)
                            .color(p.muted),
                    );
                }
            }
        });
    }

    /// MCP servers: name, command, arguments, enabled, and Test (lists the tools).
    fn mcp_settings(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        if self.mcp_args.len() != self.config.mcp_servers.len() {
            self.mcp_args = self
                .config
                .mcp_servers
                .iter()
                .map(|s| s.args.join("\n"))
                .collect();
        }
        let locked = self.worker.is_some();
        let (mut test, mut remove) = (None, None);
        Self::card(ui, |ui| {
            ui.horizontal(|ui| {
                Self::eyebrow(ui, &icons::label(icons::CONNECTIONS, "MCP SERVERS"));
                ui.label(RichText::new("ⓘ").size(11.0).color(p.muted)).on_hover_text(
                    "Tools from enabled servers are offered in Code mode as mcp__server__tool. Every call needs your approval. Servers run as your account.",
                );
            });
            ui.add_enabled_ui(!locked, |ui| {
                for (i, server) in self.config.mcp_servers.iter_mut().enumerate() {
                    ui.push_id(("mcp", i), |ui| {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut server.enabled, "")
                                .on_hover_text("Enabled");
                            ui.add(
                                egui::TextEdit::singleline(&mut server.name)
                                    .hint_text("name")
                                    .desired_width(100.0),
                            )
                            .on_hover_text("Letters, digits, - or _");
                            ui.add(
                                egui::TextEdit::singleline(&mut server.command)
                                    .hint_text("command, e.g. npx")
                                    .desired_width((ui.available_width() - 120.0).max(80.0)),
                            );
                            if ui
                                .add_enabled(!self.mcp_busy, egui::Button::new("Test"))
                                .on_hover_text("Start the server and list its tools")
                                .clicked()
                            {
                                test = Some(i);
                            }
                            if ui
                                .small_button(icons::TRASH)
                                .on_hover_text("Remove (Undo is offered)")
                                .clicked()
                            {
                                remove = Some(i);
                            }
                        });
                        if let Some(args) = self.mcp_args.get_mut(i)
                            && ui
                                .add(
                                    egui::TextEdit::multiline(args)
                                        .hint_text("Arguments, one per line")
                                        .desired_rows(1)
                                        .desired_width(f32::INFINITY),
                                )
                                .changed()
                        {
                            server.args = args
                                .lines()
                                .map(str::trim)
                                .filter(|a| !a.is_empty())
                                .map(str::to_owned)
                                .collect();
                        }
                        if let Some((name, result)) = &self.mcp_test
                            && *name == server.name
                        {
                            ui.label(RichText::new(result).size(11.5).color(p.muted));
                        }
                    });
                    ui.add_space(4.0);
                }
                if ui.button(icons::label(icons::PLUS, "Add server")).clicked() {
                    let taken: Vec<&str> = self
                        .config
                        .mcp_servers
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect();
                    let name = (1..)
                        .map(|n| format!("server{n}"))
                        .find(|n| !taken.contains(&n.as_str()))
                        .unwrap_or_default();
                    self.config.mcp_servers.push(crate::core::mcp::McpServer {
                        name,
                        ..Default::default()
                    });
                    self.mcp_args.push(String::new());
                }
            });
        });
        if let Some(i) = remove {
            let server = self.config.mcp_servers.remove(i);
            self.mcp_args.remove(i);
            let name = server.name.clone();
            self.undo_toast(
                format!("Removed MCP server {name}"),
                Box::new(move |studio: &mut Studio| {
                    let at = i.min(studio.config.mcp_servers.len());
                    studio.config.mcp_servers.insert(at, server);
                    // The argument boxes are rebuilt from the servers.
                    studio.mcp_args.clear();
                }),
                Box::new(|| {}),
            );
        }
        if let Some(i) = test {
            self.test_mcp(i);
        }
    }

    /// Extra environment variable names approved commands may see in this workspace.
    fn workspace_env_settings(&mut self, ui: &mut egui::Ui) {
        let key = self.config.workspace.display().to_string();
        if self.env_draft.0 != self.config.workspace {
            self.env_draft = (
                self.config.workspace.clone(),
                self.config
                    .workspace_env
                    .get(&key)
                    .map(|v| v.join("\n"))
                    .unwrap_or_default(),
            );
        }
        egui::CollapsingHeader::new(RichText::new("Command environment").size(12.0))
            .id_salt("workspace_env")
            .show(ui, |ui| {
                let response = ui
                    .add(
                        egui::TextEdit::multiline(&mut self.env_draft.1)
                            .hint_text("CARGO_HOME\nJAVA_HOME")
                            .desired_rows(2)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text("Names of extra variables approved commands in this workspace may see, besides PATH, HOME, TEMP and the toolchain variables. Values are read from your environment when a command runs and are never stored.");
                if response.changed() {
                    let names = parse_names(&self.env_draft.1);
                    if names.is_empty() {
                        self.config.workspace_env.remove(&key);
                    } else {
                        self.config.workspace_env.insert(key, names);
                    }
                }
            });
    }

    fn updates_card(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        Self::card(ui, |ui| {
            Self::eyebrow(ui, &icons::label(icons::DOWNLOAD, "UPDATES"));
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).strong());
                if ui
                    .add_enabled(
                        !self.update_checking,
                        egui::Button::new(icons::label(icons::REFRESH, "Check for updates")),
                    )
                    .clicked()
                {
                    self.check_updates(true);
                }
                if self.update_checking {
                    busy(ui);
                }
                if !self.update_status.is_empty() {
                    ui.label(RichText::new(&self.update_status).size(12.0).color(p.muted));
                }
                if self.update.is_some()
                    && self.update_progress.is_none()
                    && ui
                        .add_enabled(
                            self.worker.is_none() && update::self_update_blocked().is_none(),
                            egui::Button::new("Install"),
                        )
                        .clicked()
                {
                    self.install_update();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Channel");
                ui.add_enabled_ui(self.update_progress.is_none(), |ui| {
                    for (value, label, hint) in [
                        ("stable", "Stable", "Tagged releases."),
                        (
                            "nightly",
                            "Nightly",
                            "Built from every push to main. Newer, but less tested.",
                        ),
                    ] {
                        if ui
                            .selectable_label(self.config.update_channel == value, label)
                            .on_hover_text(hint)
                            .clicked()
                            && self.config.update_channel != value
                        {
                            self.config.update_channel = value.into();
                            // A release found on the other channel no longer applies.
                            self.update = None;
                            self.update_hidden = false;
                            self.update_status.clear();
                        }
                    }
                });
            });
            ui.checkbox(&mut self.config.auto_update, "Check for updates at launch")
                .on_hover_text(format!(
                    "Looks for new releases of github.com/{} every 6 hours.",
                    update::REPO
                ));
            if let Some(why) = update::self_update_blocked() {
                ui.label(RichText::new(why).size(11.5).color(p.muted));
            }
        });
    }

    fn llama_settings(&mut self, ui: &mut egui::Ui, locked: bool) {
        let p = pal(ui.ctx());
        let running = self.llama.running();
        let version = self
            .server_version
            .as_ref()
            .filter(|(path, _)| Some(path) == self.server_found.as_ref())
            .and_then(|(_, v)| v.clone());
        Self::eyebrow(ui, "BUILT-IN RUNTIME");
        let mut choose_model = false;
        let mut choose_server = false;
        let mut auto_server = false;
        ui.add_enabled_ui(!locked && !running, |ui| {
            egui::Grid::new("llama_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Model");
                    ui.horizontal(|ui| {
                        let name = self
                            .config
                            .gguf_model
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "None".into());
                        ui.add(egui::Label::new(RichText::new(name).strong()).truncate())
                            .on_hover_text(self.config.gguf_model.display().to_string());
                        choose_model = ui.small_button("Choose…").clicked();
                    });
                    ui.end_row();

                    ui.label("llama-server");
                    ui.horizontal(|ui| {
                        let found = &self.server_found;
                        ui.add(
                            egui::Label::new(
                                RichText::new(match found {
                                    Some(path) => path.display().to_string(),
                                    None => "Not found".into(),
                                })
                                .size(12.0)
                                .color(if found.is_some() {
                                    p.muted
                                } else {
                                    Color32::LIGHT_RED
                                }),
                            )
                            .truncate(),
                        );
                        choose_server = ui.small_button("Choose…").clicked();
                        auto_server = !self.config.llama_server.as_os_str().is_empty()
                            && ui.small_button("Auto").on_hover_text("Search automatically").clicked();
                    });
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.config.llama_port).range(1024..=65535))
                        .on_hover_text("The next free port is used when this one is taken.");
                    ui.end_row();

                    ui.label("CPU threads");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.config.llama_threads).range(0..=128));
                        ui.label(RichText::new("0 = auto").size(11.5).color(p.muted));
                    });
                    ui.end_row();

                    ui.label("GPU offload");
                    ui.horizontal(|ui| {
                        let mut auto = self.config.llama_gpu_layers.is_none();
                        ui.checkbox(&mut auto, "Auto").on_hover_text(if llama::INTEL_MAC {
                            "Intel Mac: runs on the CPU only (Metal on Intel GPUs is slow or unstable)."
                        } else {
                            "Uses llama-server's default."
                        });
                        if auto {
                            self.config.llama_gpu_layers = None;
                        } else {
                            let mut layers = self.config.llama_gpu_layers.unwrap_or(0);
                            ui.add(egui::DragValue::new(&mut layers).range(0..=999).suffix(" layers"));
                            self.config.llama_gpu_layers = Some(layers);
                        }
                    });
                    ui.end_row();
                });
        });
        if choose_model {
            self.choose_model_file();
        }
        if choose_server {
            self.choose_server();
        }
        if auto_server {
            self.config.llama_server = Default::default();
            self.rescan_server();
        }
        ui.horizontal_wrapped(|ui| {
            if running {
                if ui.button("Stop model").clicked() {
                    self.llama.stop();
                    self.live_port = None;
                    self.vision = None;
                    self.metrics = None;
                    self.check();
                }
                ui.label(RichText::new("Running").color(p.accent));
                if let Some(info) = self.llama.backend_info() {
                    ui.label(RichText::new(info).size(12.0).color(p.muted));
                }
            } else if self.llama_busy {
                busy(ui);
                ui.label(RichText::new("Loading model…").color(p.muted));
            } else {
                ui.add_enabled_ui(!locked && self.paths_ok[0], |ui| {
                    if Self::primary(ui, "Start model").clicked() {
                        self.start_llama();
                    }
                });
            }
            if let Some(version) = &version {
                ui.label(
                    RichText::new(format!("build {version}"))
                        .size(12.0)
                        .color(p.muted),
                );
            }
        });
        if llama::INTEL_MAC {
            ui.label(
                RichText::new(
                    "Intel Mac: CPU inference. Small quantized models (1–4B, Q4) work best.",
                )
                .size(11.5)
                .color(p.muted),
            );
        }
        egui::CollapsingHeader::new(RichText::new("External server").size(12.0))
            .id_salt("external_server")
            .show(ui, |ui| {
                ui.add_enabled_ui(!locked && !running, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config.direct_url)
                            .desired_width(f32::INFINITY),
                    )
                    .on_hover_text("Any OpenAI-compatible server. This computer only, unless a trusted host is allowed under Network & privacy.");
                });
            });
    }

    /// Model choice for this machine and the automatic download.
    fn setup_settings(&mut self, ui: &mut egui::Ui, locked: bool) {
        let p = pal(ui.ctx());
        Self::eyebrow(ui, "AUTOMATIC SETUP");
        let ram = self
            .ram_gb
            .map(|g| format!("{g} GB RAM"))
            .unwrap_or_else(|| "RAM unknown".into());
        let best = setup::best_fit();
        let also = if best.file == self.recommended.file {
            String::new()
        } else {
            format!(" This machine ({ram}) can also run {}.", best.name)
        };
        ui.label(
            RichText::new(format!(
                "Default download: {}.{also}",
                self.recommended.name
            ))
            .size(12.0)
            .color(p.muted),
        );
        let current = self.setup_choice();
        let recommended = self.recommended;
        ui.add_enabled_ui(self.setup.is_none(), |ui| {
            egui::ComboBox::from_id_salt("setup_model")
                .selected_text(current.name)
                .width(260.0)
                .show_ui(ui, |ui| {
                    for m in setup::MODELS {
                        let label = format!("{} · {:.1} GB", m.name, m.size as f64 / 1e9);
                        let response = ui.selectable_label(m.file == current.file, label);
                        let response = if m.min_ram_gb > 0 {
                            response
                                .on_hover_text(format!("Needs about {} GB of RAM", m.min_ram_gb))
                        } else {
                            response
                        };
                        if response.clicked() {
                            self.config.setup_model = if m.file == recommended.file {
                                String::new()
                            } else {
                                m.file.into()
                            };
                        }
                    }
                })
                .response
                .on_hover_text(format!("Saved in {}", setup::models_dir().display()));
        });
        if self.setup.is_some() {
            self.setup_progress(ui);
        } else if ui
            .add_enabled(
                !locked && !self.llama.running(),
                egui::Button::new("Set up automatically"),
            )
            .on_hover_text(format!(
                "Downloads llama.cpp {} if needed and the chosen model, then starts it.",
                setup::LLAMA_TAG
            ))
            .on_disabled_hover_text("Stop the model and wait for running work to finish.")
            .clicked()
        {
            self.start_setup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metrics_readout() {
        let m = llama::Metrics {
            ram_mb: 1536,
            ctx_used: 2100,
            ctx_total: 8192,
            tokens_per_sec: Some(12.34),
        };
        assert_eq!(metrics_text(&m), "12.3 tok/s · 2.1k/8.2k ctx · 1.5 GB");
        let idle = llama::Metrics {
            ram_mb: 900,
            ..Default::default()
        };
        assert_eq!(metrics_text(&idle), "900 MB");
        assert_eq!(metrics_text(&llama::Metrics::default()), "");
    }
    #[test]
    fn env_names_split_on_lines_commas_and_spaces() {
        assert_eq!(
            parse_names("CARGO_HOME, JAVA_HOME\n\n  GOPATH "),
            vec!["CARGO_HOME", "JAVA_HOME", "GOPATH"]
        );
        assert!(parse_names("  \n").is_empty());
    }
    #[test]
    fn height_estimate_grows_with_text() {
        let short = estimate_height("hi", 800.0);
        let long = estimate_height(&"word ".repeat(400), 800.0);
        let lines = estimate_height(&"x\n".repeat(30), 800.0);
        assert!(short > 40.0 && short < 80.0, "{short}");
        assert!(long > short * 3.0, "{long}");
        assert!(lines > 500.0, "{lines}");
        assert!(estimate_height("", 100.0) > 0.0);
    }
    /// Estimating the off-screen part of a long chat must stay far cheaper than parsing it.
    #[test]
    fn perf_estimate_long_chat() {
        let reply = "Some prose that wraps across the bubble width.\n".repeat(40)
            + "```rust\nfn main() { println!(\"hi\"); }\n```\n";
        let chat: Vec<String> = (0..500).map(|_| reply.clone()).collect();
        let start = std::time::Instant::now();
        let total: f32 = chat.iter().map(|m| estimate_height(m, 860.0)).sum();
        let took = start.elapsed();
        eprintln!(
            "perf_estimate_long_chat: 500 messages ({} KB) in {took:?}",
            chat.iter().map(String::len).sum::<usize>() / 1024
        );
        assert!(total > 0.0);
        assert!(
            took < std::time::Duration::from_millis(200),
            "took {took:?}"
        );
    }
}

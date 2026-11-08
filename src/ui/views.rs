use super::*;

/// Sidebar navigation: icon, label and the view index it opens.
const NAV: [(&str, &str, usize); 5] = [
    ("💬", "Chat", 0),
    ("📚", "Library", 1),
    ("👥", "Characters", 4),
    ("🔌", "Connections", 2),
    ("⚙", "Settings", 3),
];

/// What a click in the message list asks for, applied after the list is drawn.
enum MessageAction {
    Regenerate,
    Edit(usize),
    Resend(usize),
    CancelEdit,
    Select(usize),
}
enum Fix {
    Model,
    Server,
    Start,
    Ollama,
    Workspace,
    Docs,
}

/// A frameless, muted text button used in message headers.
fn quiet_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = pal(ui.ctx());
    ui.add(egui::Button::new(RichText::new(text).size(11.0).color(p.muted)).frame(false))
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
                                RichText::new("+  New chat").color(p.on_accent).strong(),
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
                for (_, label, view) in NAV {
                    let selected = self.view == view;
                    let button = egui::Button::new(RichText::new(label).color(if selected {
                        p.accent
                    } else {
                        p.text
                    }))
                    .fill(if selected { p.selected } else { p.panel })
                    .corner_radius(6);
                    if ui.add_sized([width, 28.0], button).clicked() {
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
                            RichText::new("+").color(p.on_accent).strong().size(16.0),
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
                    .hint_text("Search chats")
                    .desired_width(width),
            );
        }
        let query = self.conversation_search.trim().to_lowercase();
        let idle = self.worker.is_none();
        let (mut open, mut delete, mut rename, mut renamed) = (None, None, None, None);
        egui::ScrollArea::vertical()
            .id_salt("chats")
            .max_height(max_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if self.conversations.is_empty() {
                    ui.label(RichText::new("No saved chats").size(11.5).color(p.muted));
                }
                for meta in &self.conversations {
                    if !query.is_empty() && !meta.title.to_lowercase().contains(&query) {
                        continue;
                    }
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
                    ui.horizontal(|ui| {
                        let button = egui::Button::new(
                            RichText::new(&meta.title).size(12.5).color(if selected {
                                p.accent
                            } else {
                                p.text
                            }),
                        )
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
                            open = Some(meta.id.clone());
                        }
                        ui.menu_button(RichText::new("…").color(p.muted), |ui| {
                            if ui.button("Rename").clicked() {
                                rename = Some((meta.id.clone(), meta.title.clone()));
                                ui.close();
                            }
                            if ui.add_enabled(idle, egui::Button::new("Delete")).clicked() {
                                delete = Some(meta.id.clone());
                                ui.close();
                            }
                        });
                    });
                }
            });
        if let Some(result) = renamed {
            self.renaming = None;
            if let Some((id, title)) = result {
                self.rename_conversation(&id, title);
            }
        }
        if rename.is_some() {
            self.renaming = rename;
        }
        if let Some(id) = open {
            self.open_conversation(&id);
        }
        if let Some(id) = delete {
            self.delete_conversation(&id);
        }
    }

    fn project_files(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        ui.horizontal(|ui| {
            Self::eyebrow(ui, "PROJECT");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(self.worker.is_none(), |ui| {
                    if ui.small_button("Open…").on_hover_text("Ctrl+O").clicked() {
                        self.choose_workspace();
                    }
                });
            });
        });
        ui.add(
            egui::Label::new(
                RichText::new(
                    self.config
                        .workspace
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                )
                .strong(),
            )
            .truncate(),
        )
        .on_hover_text(self.config.workspace.display().to_string());
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("files")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let file_button = |ui: &mut egui::Ui, name: &str, full: &str| {
                    ui.add(
                        egui::Button::new(RichText::new(name).size(11.5).color(p.muted))
                            .frame(false)
                            .truncate(),
                    )
                    .on_hover_text(full)
                    .clicked()
                };
                for (dir, items) in &self.file_groups {
                    if dir.is_empty() {
                        for (name, full) in items {
                            if file_button(ui, name, full) {
                                clicked = Some(full.clone());
                            }
                        }
                    } else {
                        egui::CollapsingHeader::new(
                            RichText::new(format!("{dir}/")).size(11.5).color(p.text),
                        )
                        .id_salt(("dir", dir))
                        .default_open(false)
                        .show(ui, |ui| {
                            for (name, full) in items {
                                if file_button(ui, name, full) {
                                    clicked = Some(full.clone());
                                }
                            }
                        });
                    }
                }
                if self.files.is_empty() {
                    ui.label(RichText::new("No files").size(11.5).color(p.muted));
                }
            });
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
                        ui.label(RichText::new(&m.role).size(10.5).color(p.accent));
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
                        if ui.small_button("Copy").clicked() {
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
        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(idle, |ui| {
                ui.selectable_value(&mut self.coding, false, "Chat");
                ui.selectable_value(&mut self.coding, true, "Code");
                ui.separator();
                egui::ComboBox::from_id_salt("model")
                    .selected_text(if self.config.model.is_empty() || self.models.is_empty() {
                        "No model loaded"
                    } else {
                        &self.config.model
                    })
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for model in &self.models {
                            ui.selectable_value(&mut self.config.model, model.clone(), model);
                        }
                    })
                    .response
                    .on_hover_text("Model");
                if !self.config.personas.is_empty() {
                    let name = self
                        .config
                        .personas
                        .get(self.active_persona)
                        .map(|p| p.name.as_str())
                        .unwrap_or("Assistant");
                    egui::ComboBox::from_id_salt("character")
                        .selected_text(name)
                        .width(130.0)
                        .show_ui(ui, |ui| {
                            for (i, p) in self.config.personas.iter().enumerate() {
                                ui.selectable_value(&mut self.active_persona, i, &p.name);
                            }
                        })
                        .response
                        .on_hover_text("Character");
                }
                ui.menu_button("Options", |ui| {
                    ui.checkbox(&mut self.use_docs, "Use documentation");
                    ui.checkbox(&mut self.use_embeddings, "Embedding rerank")
                        .on_hover_text("Rerank document matches with the Ollama embedding model chosen in Settings.");
                    ui.checkbox(&mut self.use_web, "Web search")
                        .on_hover_text("Sends search queries to DuckDuckGo.");
                    ui.add_enabled(
                        !self.coding,
                        egui::Checkbox::new(&mut self.group_chat, "Group chat"),
                    );
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
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.config.show_inspector, "Context")
                    .on_hover_text("Show sources and activity")
                    .clicked()
                {
                    self.config.show_inspector = !self.config.show_inspector;
                }
            });
        });
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
            let prompts: &[(&str, &str)] = if self.coding {
                &[
                    ("Explore this project", "List the files in this workspace and explain how the project is organized. Read the main files first."),
                    ("Ask the docs", "Explain Rust ownership and borrowing with a short example. Cite the documentation."),
                    ("Plan a feature", "Help me plan a new feature. First inspect the workspace, then ask what I want to build."),
                ]
            } else {
                &[
                    ("Brainstorm", "Help me brainstorm ideas. Ask me a couple of questions first."),
                    ("Explain a concept", "Explain a concept to me simply, then check my understanding."),
                ]
            };
            // Measure the chip row so it can be centered as a unit.
            let spacing = ui.spacing().item_spacing.x;
            let pad = ui.spacing().button_padding.x * 2.0;
            let row: f32 = prompts
                .iter()
                .map(|(t, _)| {
                    ui.fonts_mut(|f| {
                        f.layout_no_wrap(
                            (*t).to_owned(),
                            egui::TextStyle::Button.resolve(ui.style()),
                            p.text,
                        )
                        .size()
                        .x
                    }) + pad
                })
                .sum::<f32>()
                + spacing * (prompts.len().saturating_sub(1)) as f32;
            ui.horizontal(|ui| {
                ui.add_space(((ui.available_width() - row) / 2.0).max(0.0));
                for (title, prompt) in prompts {
                    if ui
                        .add(egui::Button::new(*title).fill(p.card).corner_radius(8))
                        .clicked()
                    {
                        self.draft = (*prompt).into();
                    }
                }
            });
        });
    }

    /// First-run checklist shown in the empty chat while no model answers.
    fn setup_checklist(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let direct = self.config.direct_backend;
        let model_ok = self.config.gguf_model.is_file();
        let server_ok = self.server_found.is_some();
        let busy = self.llama_busy || self.setup.is_some();
        let mut rows: Vec<(bool, &str, &str, Fix, bool)> = vec![];
        if direct {
            rows.push((model_ok, "Model file", "Choose…", Fix::Model, !busy));
            rows.push((
                server_ok,
                "llama-server found",
                "Choose…",
                Fix::Server,
                !busy,
            ));
            rows.push((
                self.online || self.llama.running(),
                "Model running",
                "Start",
                Fix::Start,
                model_ok && server_ok && !busy,
            ));
        } else {
            rows.push((
                self.online,
                "Ollama running",
                "Start",
                Fix::Ollama,
                !self.checking,
            ));
        }
        rows.push((
            self.config.workspace.is_dir(),
            "Workspace",
            "Open…",
            Fix::Workspace,
            self.worker.is_none(),
        ));
        rows.push((
            self.config.docs.is_dir(),
            "Docs folder (optional)",
            "Choose…",
            Fix::Docs,
            true,
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
                    for (ok, label, button, action, enabled) in rows {
                        ui.horizontal(|ui| {
                            // Fixed-width icon column so labels line up whatever the glyph.
                            ui.add_sized(
                                [16.0, ui.spacing().interact_size.y],
                                egui::Label::new(
                                    RichText::new(if ok { "✔" } else { "–" }).color(if ok {
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
                                            .add_enabled(enabled, egui::Button::new(button))
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
                            ui.spinner();
                            ui.label(RichText::new("Loading model…").color(p.muted));
                        });
                    } else if direct && (!model_ok || !server_ok) {
                        ui.separator();
                        let model = self.setup_choice();
                        auto = Self::primary(ui, "Set up automatically")
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
            Some(Fix::Docs) => self.choose_docs(),
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
            self.server_found = llama::find_server(&self.config);
            self.persist();
        }
    }

    fn message_list(&mut self, ui: &mut egui::Ui, width: f32, viewport: egui::Rect, origin: f32) {
        let p = pal(ui.ctx());
        let count = self.conversation.messages.len();
        let idle = self.worker.is_none();
        let mut action = None;
        for i in 0..count {
            let editing = matches!(&self.editing, Some((e, _)) if *e == i);
            let m = &mut self.conversation.messages[i];
            // Off-screen messages with a known size are skipped, keeping only their space.
            let y = ui.cursor().top() - origin;
            let size = m.cache.size;
            let fresh = m.cache.parsed_len == Some(m.text.len());
            if size.y > 0.0
                && fresh
                && !editing
                && (y + size.y < viewport.min.y - 400.0 || y > viewport.max.y + 400.0)
            {
                ui.allocate_exact_size(egui::vec2(size.x.min(width), size.y), egui::Sense::hover());
                ui.add_space(8.0);
                continue;
            }
            let blocks = m.blocks();
            let mine = m.role == "You";
            let streaming = !idle && i + 1 == count;
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
                        ui.label(RichText::new(&m.role).size(12.0).strong().color(if mine {
                            p.accent
                        } else {
                            p.muted
                        }));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !m.text.is_empty() && quiet_button(ui, "Copy").clicked() {
                                ui.ctx().copy_text(m.text.clone());
                            }
                            if idle && mine && !editing && quiet_button(ui, "Edit").clicked() {
                                action = Some(MessageAction::Edit(i));
                            }
                            if idle
                                && !mine
                                && i + 1 == count
                                && quiet_button(ui, "Regenerate").clicked()
                            {
                                action = Some(MessageAction::Regenerate);
                            }
                            if !m.sources.is_empty()
                                && quiet_button(ui, &format!("Sources ({})", m.sources.len()))
                                    .clicked()
                            {
                                action = Some(MessageAction::Select(i));
                            }
                        });
                    });
                    if let Some((_, text)) = self.editing.as_mut().filter(|_| editing) {
                        ui.add(
                            egui::TextEdit::multiline(text)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            if Self::primary(ui, "Send").clicked() {
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
                            ui.spinner();
                            ui.label(RichText::new(&self.status).color(p.muted));
                        });
                    } else {
                        render_blocks(ui, &blocks, i);
                    }
                })
                .response;
            m.cache.size = response.rect.size();
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
        } else {
            None
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let id = egui::Id::new("draft_input");
        let idle = self.worker.is_none();
        // Enter sends and Shift+Enter adds a line: take Enter before the text box sees it.
        let submit = idle
            && self.online
            && ui.memory(|m| m.has_focus(id))
            && ui.input_mut(|i| {
                (!i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
                    || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
            });
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
        let mut stop = false;
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&self.status).size(11.0).color(p.muted)).truncate(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !idle {
                    stop = ui.button("Stop").on_hover_text("Esc").clicked();
                } else {
                    let blocked = self.send_blocked();
                    let button =
                        egui::Button::new(RichText::new("Send").color(p.on_accent).strong())
                            .fill(p.accent)
                            .corner_radius(8);
                    send |= ui
                        .add_enabled(blocked.is_none(), button)
                        .on_disabled_hover_text(blocked.unwrap_or_default())
                        .clicked();
                }
            });
        });
        if stop {
            self.stop();
        } else if send {
            self.send();
        }
    }

    pub(super) fn library_ui(&mut self, ui: &mut egui::Ui) {
        let pl = pal(ui.ctx());
        Self::page_title(ui, "Library");
        let p = self.library.progress.lock().unwrap().clone();
        let mut choose = false;
        Self::card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} files · {} passages", p.files, p.chunks)).strong(),
                )
                .on_hover_text("Markdown, MDX, text, RST, AsciiDoc and HTML are indexed.");
                if p.running {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    choose = ui
                        .add_enabled(
                            !p.running && self.worker.is_none(),
                            egui::Button::new("Choose folder…"),
                        )
                        .clicked();
                    if ui
                        .add_enabled(!p.running, egui::Button::new("Rebuild"))
                        .clicked()
                    {
                        self.library.rebuild(self.config.docs.clone());
                    }
                });
            });
            ui.label(
                RichText::new(self.config.docs.display().to_string())
                    .size(11.5)
                    .color(pl.muted),
            );
            let mut note = p.message.clone();
            if p.skipped > 0 {
                note.push_str(&format!(" · {} skipped", p.skipped));
            }
            if !note.trim().is_empty() {
                ui.label(RichText::new(note).size(11.5).color(pl.muted));
            }
            if !p.skipped_files.is_empty() {
                egui::CollapsingHeader::new(format!("Skipped files ({})", p.skipped))
                    .id_salt("skipped_files")
                    .show(ui, |ui| {
                        for (path, reason) in &p.skipped_files {
                            ui.label(
                                RichText::new(format!("{path}: {reason}"))
                                    .size(11.5)
                                    .color(pl.muted),
                            );
                        }
                        if p.skipped > p.skipped_files.len() {
                            ui.label(
                                RichText::new(format!(
                                    "…and {} more",
                                    p.skipped - p.skipped_files.len()
                                ))
                                .size(11.5)
                                .color(pl.muted),
                            );
                        }
                    });
            }
        });
        if choose {
            self.choose_docs();
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text(if self.pack.is_empty() {
                        "Search documentation".to_owned()
                    } else {
                        format!("Search {}", self.pack)
                    })
                    .desired_width((ui.available_width() - 80.0).max(100.0)),
            );
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.button("Search").clicked() || enter {
                if self.library.matches_root(&self.config.docs) {
                    match self.library.search(&self.search, &self.pack, 8) {
                        Ok(s) => self.search_results = s,
                        Err(e) => self.error = Some(e.to_string()),
                    }
                } else {
                    self.error = Some("Wait for the index to finish for this folder.".into());
                }
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt("library_results")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, s) in self.search_results.iter().enumerate() {
                    Self::card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("[{}] {}:{}", i + 1, s.path, s.line))
                                    .strong()
                                    .color(pl.accent),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Copy").clicked() {
                                        ui.ctx().copy_text(s.text.clone());
                                    }
                                },
                            );
                        });
                        ui.label(&s.text);
                    });
                    ui.add_space(8.0);
                }
            });
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
            Self::eyebrow(ui, "MODEL PROVIDER");
            ui.add_enabled_ui(!locked, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.config.direct_backend, true, "llama.cpp");
                    ui.selectable_value(&mut self.config.direct_backend, false, "Ollama")
                        .on_hover_text("Optional. Also used for embedding rerank.");
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
                    ui.spinner();
                }
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.checking, egui::Button::new("Check connection"))
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
            Self::eyebrow(ui, "GENERATION");
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
            });
        });
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "INTERFACE");
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
            ui.checkbox(
                &mut self.config.show_inspector,
                "Show context panel in Chat",
            );
            ui.checkbox(&mut self.config.sidebar_collapsed, "Collapse sidebar")
                .on_hover_text("Ctrl+B");
        });
        ui.add_space(12.0);
        self.updates_card(ui);
        ui.add_space(12.0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "PROJECT & DATA");
            ui.horizontal_wrapped(|ui| {
                ui.label(self.config.workspace.display().to_string())
                    .on_hover_text("File tools are limited to this folder. Approved commands run as your account.");
                if ui
                    .add_enabled(self.worker.is_none(), egui::Button::new("Change…"))
                    .clicked()
                {
                    self.choose_workspace();
                }
            });
            ui.label(
                RichText::new(data_dir().display().to_string())
                    .size(11.5)
                    .color(p.muted),
            )
            .on_hover_text("Settings, conversations, search index and file backups.");
            ui.horizontal_wrapped(|ui| {
                if ui.button("Save settings").clicked() {
                    self.persist();
                    self.status = "Settings saved".into();
                }
                if ui.button("Export conversation…").clicked()
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
                        Ok(()) => self.status = "Conversation exported".into(),
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if ui
                    .button("Copy diagnostics")
                    .on_hover_text(
                        "Versions, paths and recent log lines, for a bug report. Nothing is sent.",
                    )
                    .clicked()
                {
                    ui.ctx().copy_text(diagnostics::report());
                    self.status = "Diagnostics copied".into();
                }
            });
        });
    }

    fn updates_card(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "UPDATES");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).strong());
                if ui
                    .add_enabled(
                        !self.update_checking,
                        egui::Button::new("Check for updates"),
                    )
                    .clicked()
                {
                    self.check_updates(true);
                }
                if self.update_checking {
                    ui.spinner();
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
                        if !self.config.llama_server.as_os_str().is_empty()
                            && ui.small_button("Auto").on_hover_text("Search automatically").clicked()
                        {
                            self.config.llama_server = Default::default();
                            self.server_found = llama::find_server(&self.config);
                        }
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
        ui.horizontal_wrapped(|ui| {
            if running {
                if ui.button("Stop model").clicked() {
                    self.llama.stop();
                    self.check();
                }
                ui.label(RichText::new("Running").color(p.accent));
                if let Some(info) = self.llama.backend_info() {
                    ui.label(RichText::new(info).size(12.0).color(p.muted));
                }
            } else if self.llama_busy {
                ui.spinner();
                ui.label(RichText::new("Loading model…").color(p.muted));
            } else {
                ui.add_enabled_ui(!locked && self.config.gguf_model.is_file(), |ui| {
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
                    .on_hover_text("Any OpenAI-compatible server on localhost.");
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
        ui.label(
            RichText::new(format!(
                "Recommended for this machine ({ram}): {}",
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

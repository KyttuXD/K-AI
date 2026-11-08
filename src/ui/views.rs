use super::*;
use crate::core::llama;

/// Sidebar navigation: label and the view index it opens.
const NAV: [(&str, usize); 5] = [
    ("Chat", 0),
    ("Library", 1),
    ("Characters", 4),
    ("Connections", 2),
    ("Settings", 3),
];

impl Studio {
    pub(super) fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .default_width(196.0)
            .width_range(160.0..=300.0)
            .resizable(true)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(egui::Margin::symmetric(12, 14)),
            )
            .show(ctx, |ui| {
                egui::TopBottomPanel::bottom("sidebar_footer")
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, 6)))
                    .show_separator_line(false)
                    .show_inside(ui, |ui| {
                        let (text, color) = if !self.online {
                            ("Offline", Color32::LIGHT_RED)
                        } else if self.config.direct_backend {
                            ("llama.cpp", ACCENT)
                        } else {
                            ("Ollama", ACCENT)
                        };
                        ui.horizontal(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                            ui.painter().circle_filled(rect.center(), 4.0, color);
                            ui.label(RichText::new(text).color(color).size(12.0));
                        })
                        .response
                        .on_hover_text(&self.connection);
                    });
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), egui::Sense::hover());
                    ui.painter().rect_filled(rect, 6.0, ACCENT);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "K",
                        FontId::proportional(16.0),
                        PANEL,
                    );
                    ui.label(RichText::new("KYTTU").size(16.0).strong());
                });
                ui.add_space(12.0);
                let width = ui.available_width();
                ui.add_enabled_ui(self.worker.is_none(), |ui| {
                    if ui
                        .add_sized(
                            [width, 30.0],
                            egui::Button::new(RichText::new("+  New chat").color(PANEL).strong())
                                .fill(ACCENT)
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
                for (label, view) in NAV {
                    let selected = self.view == view;
                    let button = egui::Button::new(RichText::new(label).color(if selected {
                        ACCENT
                    } else {
                        TEXT
                    }))
                    .fill(if selected { SELECTED } else { PANEL })
                    .corner_radius(6);
                    if ui.add_sized([width, 28.0], button).clicked() {
                        self.view = view;
                    }
                }
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.add_space(14.0);
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
                egui::ScrollArea::vertical()
                    .id_salt("files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        for file in &self.files {
                            if ui
                                .add(
                                    egui::Label::new(RichText::new(file).size(11.5).color(MUTED))
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                self.preview = Some((
                                    file.clone(),
                                    tools::read(&self.config.workspace, file)
                                        .unwrap_or_else(|e| e.to_string()),
                                ));
                            }
                        }
                        if self.files.is_empty() {
                            ui.label(RichText::new("No files").size(11.5).color(MUTED));
                        }
                    });
            });
    }

    pub(super) fn inspector(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("inspector")
            .default_width(260.0)
            .width_range(200.0..=440.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .inner_margin(14),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("inspector_scroll")
                    .show(ui, |ui| {
                        Self::eyebrow(ui, "SOURCES");
                        if self.sources.is_empty() {
                            ui.label(RichText::new("None yet").color(MUTED).size(12.0));
                        }
                        for (i, source) in self.sources.iter().enumerate() {
                            egui::CollapsingHeader::new(
                                RichText::new(format!("[{}] {}", i + 1, source.path)).size(12.0),
                            )
                            .id_salt((i, &source.path))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("line {}", source.line))
                                        .size(10.5)
                                        .color(MUTED),
                                );
                                if source.path.starts_with("https://")
                                    || source.path.starts_with("http://")
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
                            ui.label(RichText::new("None yet").color(MUTED).size(12.0));
                        }
                        for activity in &self.activity {
                            ui.label(RichText::new(activity).size(11.5).color(MUTED));
                        }
                    });
            });
    }

    pub(super) fn conversation_ui(&mut self, ui: &mut egui::Ui) {
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
            .fill(PANEL)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(12)
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::TopBottomPanel::bottom("composer")
                    .frame(
                        egui::Frame::new()
                            .fill(CARD)
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
            .show(ui, |ui| {
                // A centered column, capped so lines stay readable on wide windows.
                let full = ui.available_width();
                let width = full.min(860.0);
                let empty_height = ui.available_height();
                ui.horizontal_top(|ui| {
                    ui.add_space(((full - width) / 2.0).max(0.0));
                    ui.vertical(|ui| {
                        ui.set_width(width);
                        if self.conversation.messages.is_empty() {
                            self.empty_state(ui, empty_height);
                        } else {
                            self.message_list(ui, width);
                        }
                    });
                });
            });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui, height: f32) {
        // Roughly vertically centered: the title + chips block is ~80pt tall.
        ui.add_space(((height - 80.0) / 2.0 - 24.0).max(16.0));
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
                                    TEXT,
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
                                .add(egui::Button::new(*title).fill(CARD).corner_radius(8))
                                .clicked()
                            {
                                self.draft = (*prompt).into();
                            }
                        }
                    });
        });
    }

    fn message_list(&mut self, ui: &mut egui::Ui, width: f32) {
        let last = self.conversation.messages.len();
        for (i, m) in self.conversation.messages.iter().enumerate() {
            let mine = m.role == "You";
            egui::Frame::new()
                .fill(if mine { USER_BUBBLE } else { CARD })
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(14, 10))
                .show(ui, |ui| {
                    ui.set_min_width(width - 30.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&m.role).size(12.0).strong().color(if mine {
                            ACCENT
                        } else {
                            MUTED
                        }));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !m.text.is_empty()
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Copy").size(11.0).color(MUTED),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                            {
                                ui.ctx().copy_text(m.text.clone());
                            }
                        });
                    });
                    if m.text.is_empty() && self.worker.is_some() && i + 1 == last {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(&self.status).color(MUTED));
                        });
                    } else {
                        markdown(ui, &m.text, i);
                    }
                });
            ui.add_space(8.0);
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new().show(ui, |ui| {
            let response = egui::ScrollArea::vertical()
                .id_salt("draft")
                .max_height(160.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.draft)
                            .hint_text("Message  ·  Ctrl+Enter to send")
                            .desired_rows(2)
                            .desired_width(f32::INFINITY)
                            .frame(false),
                    )
                })
                .inner;
            let shortcut = response.has_focus()
                && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(RichText::new(&self.status).size(11.0).color(MUTED))
                        .truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.worker.is_some() {
                        if ui.button("Stop").clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                            self.status = "Stopping…".into();
                            if let Some(a) = self.approval.take() {
                                let _ = a.reply.send(false);
                            }
                        }
                    } else {
                        ui.add_enabled_ui(self.online && !self.draft.trim().is_empty(), |ui| {
                            if Self::primary(ui, "Send").clicked() || shortcut {
                                self.send();
                            }
                        });
                    }
                });
            });
        });
    }

    pub(super) fn library_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Library");
        let p = self.library.progress.lock().unwrap().clone();
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
                    ui.add_enabled_ui(!p.running && self.worker.is_none(), |ui| {
                        if ui.button("Choose folder…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
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
                    });
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
                    .color(MUTED),
            );
            let mut note = p.message.clone();
            if p.skipped > 0 {
                note.push_str(&format!(" · {} skipped", p.skipped));
            }
            if !note.trim().is_empty() {
                ui.label(RichText::new(note).size(11.5).color(MUTED));
            }
        });
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
                                    .color(ACCENT),
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
        let locked = self.worker.is_some()
            || self.checking
            || self.llama_busy
            || self.discord.running.load(Ordering::Relaxed);
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
                    .color(MUTED),
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
                    .color(MUTED),
                );
            } else {
                ui.label(
                    RichText::new(self.models.join(" · "))
                        .size(12.0)
                        .color(MUTED),
                );
            }
            if self.config.direct_backend {
                ui.add_space(6.0);
                ui.separator();
                self.llama_settings(ui, locked);
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
        });
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
                    .color(MUTED),
            )
            .on_hover_text("Settings, conversation, search index and file backups.");
            ui.horizontal(|ui| {
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
            });
        });
    }

    fn llama_settings(&mut self, ui: &mut egui::Ui, locked: bool) {
        let running = self.llama.running();
        Self::eyebrow(ui, "BUILT-IN RUNTIME");
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
                        if ui.small_button("Choose…").clicked()
                            && let Some(path) = rfd::FileDialog::new()
                                .add_filter("GGUF model", &["gguf"])
                                .pick_file()
                        {
                            self.config.gguf_model = path;
                        }
                    });
                    ui.end_row();

                    ui.label("llama-server");
                    ui.horizontal(|ui| {
                        let found = llama::find_server(&self.config);
                        ui.add(
                            egui::Label::new(
                                RichText::new(match &found {
                                    Some(p) => p.display().to_string(),
                                    None => "Not found".into(),
                                })
                                .size(12.0)
                                .color(if found.is_some() {
                                    MUTED
                                } else {
                                    Color32::LIGHT_RED
                                }),
                            )
                            .truncate(),
                        );
                        if ui.small_button("Choose…").clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_file()
                        {
                            self.config.llama_server = path;
                        }
                        if !self.config.llama_server.as_os_str().is_empty()
                            && ui.small_button("Auto").on_hover_text("Search automatically").clicked()
                        {
                            self.config.llama_server = Default::default();
                        }
                    });
                    ui.end_row();

                    ui.label("Port");
                    ui.add(egui::DragValue::new(&mut self.config.llama_port).range(1024..=65535));
                    ui.end_row();

                    ui.label("CPU threads");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.config.llama_threads).range(0..=128));
                        ui.label(RichText::new("0 = auto").size(11.5).color(MUTED));
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
        ui.horizontal(|ui| {
            if running {
                if ui.button("Stop model").clicked() {
                    self.llama.stop();
                    self.check();
                }
                ui.label(RichText::new("Running").color(ACCENT));
            } else if self.llama_busy {
                ui.spinner();
                ui.label(RichText::new("Loading model…").color(MUTED));
            } else {
                ui.add_enabled_ui(!locked && self.config.gguf_model.is_file(), |ui| {
                    if Self::primary(ui, "Start model").clicked() {
                        self.start_llama();
                    }
                });
            }
        });
        if llama::INTEL_MAC {
            ui.label(
                RichText::new(
                    "Intel Mac: CPU inference. Small quantized models (1–4B, Q4) work best.",
                )
                .size(11.5)
                .color(MUTED),
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
}

use super::*;

impl Studio {
    pub(super) fn characters_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Characters");
        if self.config.personas.is_empty() {
            ui.label(RichText::new("No characters yet.").color(MUTED));
        }
        ui.add_enabled_ui(self.worker.is_none(), |ui| {
            ui.horizontal(|ui| {
                if Self::primary(ui, "+ New character").clicked() {
                    self.config.personas.push(Persona {
                        name: format!("Character {}", self.config.personas.len() + 1),
                        ..Default::default()
                    });
                    self.active_persona = self.config.personas.len() - 1;
                }
                if ui.button("Save").clicked() {
                    match crate::core::personas::validate(&self.config.personas) {
                        Ok(()) => {
                            self.persist();
                            self.status = "Characters saved".into();
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if ui.button("Export…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_file_name("characters.json")
                        .save_file()
                {
                    match serde_json::to_vec_pretty(&self.config.personas)
                        .map_err(anyhow::Error::from)
                        .and_then(|data| {
                            std::fs::write(path, data)?;
                            Ok(())
                        }) {
                        Ok(()) => {}
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                if ui.button("Import…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Characters", &["json"])
                        .pick_file()
                {
                    let result = (|| -> anyhow::Result<Vec<Persona>> {
                        anyhow::ensure!(
                            std::fs::metadata(&path)?.len() <= 256_000,
                            "Character file exceeds 256 KB"
                        );
                        let personas: Vec<Persona> = serde_json::from_slice(&std::fs::read(path)?)?;
                        crate::core::personas::validate(&personas)?;
                        Ok(personas)
                    })();
                    match result {
                        Ok(personas) => {
                            self.config.personas = personas;
                            self.active_persona = 0;
                            self.persist();
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            });
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("character_editor")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut remove = None;
                    let count = self.config.personas.len();
                    for (i, persona) in self.config.personas.iter_mut().enumerate() {
                        Self::card(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut persona.name)
                                        .hint_text("Name")
                                        .font(egui::TextStyle::Heading)
                                        .desired_width(220.0)
                                        .char_limit(64),
                                );
                                ui.checkbox(&mut persona.in_group, "In group chats");
                            });
                            ui.add(
                                egui::TextEdit::multiline(&mut persona.instructions)
                                    .hint_text("Personality and instructions")
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY)
                                    .char_limit(4000),
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.label("Model");
                                egui::ComboBox::from_id_salt(("persona_model", i))
                                    .selected_text(if persona.model.is_empty() {
                                        "Use selected model"
                                    } else {
                                        &persona.model
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut persona.model,
                                            String::new(),
                                            "Use selected model",
                                        );
                                        for model in &self.models {
                                            ui.selectable_value(
                                                &mut persona.model,
                                                model.clone(),
                                                model,
                                            );
                                        }
                                    });
                                ui.add(
                                    egui::TextEdit::singleline(&mut persona.avatar_url)
                                        .hint_text("Avatar URL (https://…)")
                                        .desired_width(220.0)
                                        .char_limit(1000),
                                )
                                .on_hover_text("Optional. Used by the Discord webhook.");
                                if ui
                                    .add_enabled(count > 1, egui::Button::new("Remove"))
                                    .clicked()
                                {
                                    remove = Some(i);
                                }
                            });
                        });
                        ui.add_space(8.0);
                    }
                    if let Some(i) = remove {
                        self.config.personas.remove(i);
                        self.active_persona = self
                            .active_persona
                            .min(self.config.personas.len().saturating_sub(1));
                    }
                });
        });
    }

    pub(super) fn connections_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Connections");
        egui::ScrollArea::vertical().id_salt("connections_page").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width().min(760.0));
            Self::card(ui, |ui| {
                ui.horizontal(|ui| {
                    Self::eyebrow(ui,"DISCORD BOT");
                    let status = self.discord.status.lock().unwrap().clone();
                    ui.label(RichText::new(status).color(ACCENT).size(12.0));
                });
                let running = self.discord.running.load(Ordering::Relaxed);
                ui.add_enabled_ui(!running, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.discord_token).password(true).desired_width(f32::INFINITY).hint_text("Bot token (kept in memory only)"));
                    ui.add(egui::TextEdit::singleline(&mut self.discord_webhook).password(true).desired_width(f32::INFINITY).hint_text("Channel webhook URL (optional)"))
                        .on_hover_text("Lets characters post with their own names and avatars. Must belong to the chosen channel.");
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Channel ID"); ui.add(egui::TextEdit::singleline(&mut self.config.discord_channel).desired_width(180.0));
                        ui.label("Prefix"); ui.add(egui::TextEdit::singleline(&mut self.config.discord_prefix).desired_width(60.0).char_limit(20));
                    });
                    ui.checkbox(&mut self.config.discord_group,"Group replies by default (up to 6 characters)");
                    ui.checkbox(&mut self.config.discord_docs,"Allow local documentation in replies");
                    if self.config.discord_docs { ui.label(RichText::new("Matching documentation will be visible to everyone in the channel.").color(ACCENT).size(12.0)); }
                });
                ui.horizontal(|ui| {
                    if !running && Self::primary(ui,"Connect bot").clicked() {
                        self.persist();
                        if let Err(e) = self.discord.connect(self.discord_token.clone(),self.discord_webhook.clone(),self.config.clone(),self.library.clone()) { self.error = Some(e.to_string()); }
                    }
                    if running && ui.button("Disconnect").clicked() { self.discord.disconnect(); }
                    if !running && ui.button("Clear credentials").clicked() { self.discord_token.clear(); self.discord_webhook.clear(); }
                });
                egui::CollapsingHeader::new(RichText::new("Setup help").size(12.0)).id_salt("discord_help").show(ui, |ui| {
                    ui.label(RichText::new("Create a bot, enable Message Content Intent, invite it with View Channel and Send Messages, then copy the channel ID. It only answers prefixed messages in that channel; code tools and web search are off. Reconnect after changing characters or models.").size(12.0).color(MUTED));
                    ui.hyperlink_to("Discord developer portal","https://discord.com/developers/applications");
                    ui.monospace(format!("{p} hello\n{p} @Name review this idea\n{p} group brainstorm a feature", p = self.config.discord_prefix));
                });
            });
            ui.add_space(12.0);
            Self::card(ui, |ui| {
                Self::eyebrow(ui,"WEB SEARCH");
                ui.label(RichText::new("Turn on Web search in Chat → Options. Queries go to DuckDuckGo; local files are never uploaded.").size(12.5).color(MUTED));
            });
            ui.add_space(12.0);
            Self::card(ui, |ui| {
                Self::eyebrow(ui,"MODELS");
                ui.horizontal_wrapped(|ui| {
                    ui.hyperlink_to("Ollama library","https://ollama.com/search");
                    ui.hyperlink_to("Embedding models","https://ollama.com/search?c=embedding");
                    ui.hyperlink_to("GGUF models (Hugging Face)","https://huggingface.co/models?library=gguf");
                    ui.hyperlink_to("llama.cpp releases","https://github.com/ggml-org/llama.cpp/releases");
                });
            });
        });
    }
}

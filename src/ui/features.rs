use super::*;
use crate::core::{
    personas::{LoreEntry, MAX_LORE, MAX_LORE_TEXT, MAX_NOTE, MAX_NOTES},
    secrets,
};
use crate::integrations::discord::{self, BotProfile, MemoryScope};
use std::collections::{HashMap, HashSet};

/// A character file holds either one exported character or a whole list.
#[derive(Deserialize)]
#[serde(untagged)]
enum PersonaFile {
    Many(Vec<Persona>),
    One(Persona),
}

enum BotAction {
    Connect(usize),
    Disconnect(usize),
    Remove(usize),
}

const MAX_CHARACTER_FILE: u64 = 256_000;

impl Studio {
    pub(super) fn characters_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Characters");
        if self.config.personas.is_empty() {
            ui.label(RichText::new("No characters yet.").color(MUTED));
        }
        ui.add_enabled_ui(self.worker.is_none(), |ui| {
            ui.horizontal(|ui| {
                if Self::primary(ui, "+ New character").clicked() {
                    let taken = taken_names(&self.config.personas);
                    self.config.personas.push(Persona {
                        name: unique_name(
                            &format!("Character {}", self.config.personas.len() + 1),
                            &taken,
                        ),
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
                if ui.button("Export all…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_file_name("characters.json")
                        .save_file()
                    && let Err(e) = write_json(&path, &self.config.personas)
                {
                    self.error = Some(e.to_string());
                }
                if ui
                    .button("Import…")
                    .on_hover_text("Adds the file's characters; clashing names get a number.")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Characters", &["json"])
                        .pick_file()
                {
                    match import_personas(&path, &self.config.personas) {
                        Ok((merged, added)) => {
                            self.config.personas = merged;
                            self.persist();
                            self.status = format!("Imported {added} character(s)");
                        }
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            });
            ui.add_space(4.0);
            let confirm_id = egui::Id::new("confirm_remove_persona");
            let pending: Option<usize> = ui.data(|d| d.get_temp(confirm_id));
            egui::ScrollArea::vertical()
                .id_salt("character_editor")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let (mut remove, mut export) = (None, None);
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
                                ui.add(
                                    egui::DragValue::new(&mut persona.turn_order)
                                        .range(-99..=99)
                                        .prefix("Turn "),
                                )
                                .on_hover_text(
                                    "Group chat order: lower goes first; ties keep list order.",
                                );
                                ui.checkbox(&mut persona.discord, "Available on Discord")
                                    .on_hover_text(
                                        "Only these characters can be called by Discord bots.",
                                    );
                            });
                            ui.add(
                                egui::TextEdit::multiline(&mut persona.instructions)
                                    .hint_text("Personality and instructions")
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY)
                                    .char_limit(4000),
                            );
                            egui::CollapsingHeader::new(
                                RichText::new(format!("Lorebook ({})", persona.lorebook.len()))
                                    .size(12.0),
                            )
                            .id_salt(("lorebook", i))
                            .show(ui, |ui| lore_editor(ui, i, &mut persona.lorebook))
                            .header_response
                            .on_hover_text(
                                "Entries whose keys appear in the latest message join this \
                                 character's prompt (up to 1500 characters).",
                            );
                            egui::CollapsingHeader::new(
                                RichText::new(format!("Memory ({})", persona.memory.len()))
                                    .size(12.0),
                            )
                            .id_salt(("persona_memory", i))
                            .show(ui, |ui| notes_editor(ui, i, &mut persona.memory))
                            .header_response
                            .on_hover_text("Notes this character always keeps in mind.");
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
                                if ui.button("Export…").clicked() {
                                    export = Some(i);
                                }
                                if pending == Some(i) {
                                    ui.label(
                                        RichText::new(format!("Remove {}?", persona.name))
                                            .color(ACCENT),
                                    );
                                    if ui.button("Yes, remove").clicked() {
                                        remove = Some(i);
                                        ui.data_mut(|d| d.remove::<usize>(confirm_id));
                                    }
                                    if ui.button("Cancel").clicked() {
                                        ui.data_mut(|d| d.remove::<usize>(confirm_id));
                                    }
                                } else if ui
                                    .add_enabled(count > 1, egui::Button::new("Remove"))
                                    .clicked()
                                {
                                    ui.data_mut(|d| d.insert_temp(confirm_id, i));
                                }
                            });
                        });
                        ui.add_space(8.0);
                    }
                    if let Some(i) = export
                        && let Some(persona) = self.config.personas.get(i)
                        && let Some(path) = rfd::FileDialog::new()
                            .set_file_name(format!("{}.json", file_stem(&persona.name)))
                            .save_file()
                        && let Err(e) = write_json(&path, persona)
                    {
                        self.error = Some(e.to_string());
                    }
                    if let Some(i) = remove
                        && i < self.config.personas.len()
                    {
                        self.config.personas.remove(i);
                        if self.active_persona > i {
                            self.active_persona -= 1;
                        }
                        self.active_persona = self
                            .active_persona
                            .min(self.config.personas.len().saturating_sub(1));
                    }
                });
        });
    }

    pub(super) fn connections_ui(&mut self, ui: &mut egui::Ui) {
        Self::page_title(ui, "Connections");
        self.sync_discord_slots();
        egui::ScrollArea::vertical()
            .id_salt("connections_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(760.0));
                ui.horizontal(|ui| {
                    Self::eyebrow(ui, "DISCORD BOTS");
                    let running = self
                        .discord_sessions
                        .iter()
                        .filter(|s| s.running.load(Ordering::Relaxed))
                        .count();
                    ui.label(
                        RichText::new(format!("{running} running"))
                            .color(MUTED)
                            .size(12.0),
                    );
                });
                ui.add_space(4.0);
                let mut action = None;
                for i in 0..self.config.discord_bots.len() {
                    if let Some(a) = self.bot_card(ui, i) {
                        action = Some(a);
                    }
                    ui.add_space(8.0);
                }
                ui.horizontal(|ui| {
                    if Self::primary(ui, "+ Add bot").clicked() {
                        let taken: HashSet<String> = self
                            .config
                            .discord_bots
                            .iter()
                            .map(|b| b.name.trim().to_lowercase())
                            .collect();
                        let n = self.config.discord_bots.len() + 1;
                        self.config.discord_bots.push(BotProfile {
                            name: unique_name(&format!("Bot {n}"), &taken),
                            ..Default::default()
                        });
                        self.sync_discord_slots();
                    }
                });
                egui::CollapsingHeader::new(RichText::new("Setup help").size(12.0))
                    .id_salt("discord_help")
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "Create a bot, enable Message Content Intent, invite it with the \
                                 bot and applications.commands scopes plus View Channel and Send \
                                 Messages, and paste its channel IDs. Each bot only answers \
                                 prefixed messages and /ask, /group, /stop, /reset in its \
                                 channels, and only calls characters marked Available on \
                                 Discord. Code tools and web search are off. Reconnect after \
                                 changing characters or models.",
                            )
                            .size(12.0)
                            .color(MUTED),
                        );
                        ui.hyperlink_to(
                            "Discord developer portal",
                            "https://discord.com/developers/applications",
                        );
                        let p = self
                            .config
                            .discord_bots
                            .first()
                            .map_or("!ai", |b| b.prefix.as_str());
                        ui.monospace(format!(
                            "{p} hello\n{p} @Name review this idea\n{p} group brainstorm a feature\n\
                             /ask question: hello character: Name"
                        ));
                    });
                if let Some(a) = action {
                    self.bot_action(a);
                }
                ui.add_space(12.0);
                Self::card(ui, |ui| {
                    Self::eyebrow(ui, "WEB SEARCH");
                    ui.label(
                        RichText::new(
                            "Turn on Web search in Chat → Options. Queries go to DuckDuckGo; \
                             local files are never uploaded.",
                        )
                        .size(12.5)
                        .color(MUTED),
                    );
                });
                ui.add_space(12.0);
                Self::card(ui, |ui| {
                    Self::eyebrow(ui, "MODELS");
                    ui.horizontal_wrapped(|ui| {
                        ui.hyperlink_to("Ollama library", "https://ollama.com/search");
                        ui.hyperlink_to(
                            "Embedding models",
                            "https://ollama.com/search?c=embedding",
                        );
                        ui.hyperlink_to(
                            "GGUF models (Hugging Face)",
                            "https://huggingface.co/models?library=gguf",
                        );
                        ui.hyperlink_to(
                            "llama.cpp releases",
                            "https://github.com/ggml-org/llama.cpp/releases",
                        );
                    });
                });
            });
    }

    /// Keeps the per-bot session, token and webhook slots the same length as
    /// `config.discord_bots`, stopping any session whose profile is gone.
    fn sync_discord_slots(&mut self) {
        let n = self.config.discord_bots.len();
        for session in self.discord_sessions.iter().skip(n) {
            session.disconnect();
        }
        self.discord_sessions.truncate(n);
        self.discord_sessions.resize_with(n, Default::default);
        self.discord_tokens.resize(n, String::new());
        self.discord_webhooks.resize(n, String::new());
    }

    fn bot_card(&mut self, ui: &mut egui::Ui, i: usize) -> Option<BotAction> {
        let mut action = None;
        let running = self.discord_sessions[i].running.load(Ordering::Relaxed);
        let status = self.discord_sessions[i]
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let confirm_id = egui::Id::new("confirm_remove_bot");
        let confirming = ui.data(|d| d.get_temp::<usize>(confirm_id)) == Some(i);
        // Keychain work runs after the card, with the bot's name at that moment.
        let mut forget = false;
        Self::card(ui, |ui| {
            let bot = &mut self.config.discord_bots[i];
            ui.horizontal(|ui| {
                // Keychain entries are keyed by the name, so it is fixed while they are kept.
                let remember = bot.remember;
                ui.add_enabled(
                    !running && !remember,
                    egui::TextEdit::singleline(&mut bot.name)
                        .hint_text("Bot name")
                        .font(egui::TextStyle::Heading)
                        .desired_width(200.0)
                        .char_limit(40),
                )
                .on_disabled_hover_text(if remember {
                    "Turn off Remember to rename"
                } else {
                    "Disconnect to rename"
                });
                ui.label(RichText::new(&status).color(ACCENT).size(12.0));
            });
            ui.add_enabled_ui(!running, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.discord_tokens[i])
                        .password(true)
                        .desired_width(f32::INFINITY)
                        .hint_text(if bot.remember {
                            "Bot token (blank = use the saved one)"
                        } else {
                            "Bot token (kept in memory only)"
                        }),
                );
                if ui
                    .checkbox(&mut bot.remember, "Remember in system keychain")
                    .on_hover_text(
                        "Saves the token and webhooks in the OS keychain on Connect, never in \
                         settings. Turning this off deletes them.",
                    )
                    .changed()
                    && !bot.remember
                {
                    forget = true;
                }
                ui.label(RichText::new("Channel IDs").size(12.0).color(MUTED));
                id_list(ui, egui::Id::new(("bot_channels", i)), &mut bot.channels, "Comma or newline separated");
                ui.label(RichText::new("Server IDs (optional)").size(12.0).color(MUTED));
                id_list(ui, egui::Id::new(("bot_guilds", i)), &mut bot.guilds, "Only answer in these servers");
                let bad: Vec<&str> = bot
                    .channels
                    .iter()
                    .chain(&bot.guilds)
                    .filter(|id| !is_snowflake(id))
                    .map(String::as_str)
                    .collect();
                if !bad.is_empty() {
                    ui.label(
                        RichText::new(format!("Not a numeric ID: {}", bad.join(", ")))
                            .color(ACCENT)
                            .size(12.0),
                    );
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label("Prefix");
                    ui.add(
                        egui::TextEdit::singleline(&mut bot.prefix)
                            .desired_width(60.0)
                            .char_limit(20),
                    );
                    ui.label("Memory");
                    egui::ComboBox::from_id_salt(("bot_memory", i))
                        .selected_text(scope_label(&bot.memory))
                        .show_ui(ui, |ui| {
                            for scope in [MemoryScope::Channel, MemoryScope::Server, MemoryScope::User]
                            {
                                let label = scope_label(&scope);
                                ui.selectable_value(&mut bot.memory, scope, label);
                            }
                        })
                        .response
                        .on_hover_text("What shares one conversation history for this bot.");
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Cooldown");
                    ui.add(egui::DragValue::new(&mut bot.cooldown_secs).range(0..=600).suffix(" s"))
                        .on_hover_text("Minimum time between replies to one user.");
                    ui.label("Reply time limit");
                    ui.add(
                        egui::DragValue::new(&mut bot.reply_timeout_secs)
                            .range(10..=900)
                            .suffix(" s"),
                    );
                });
                ui.checkbox(&mut bot.group, "Group replies by default (up to 6 characters)");
                ui.checkbox(&mut bot.docs, "Allow local documentation in replies");
                if bot.docs {
                    ui.label(
                        RichText::new("Matching documentation will be visible to everyone in the channel.")
                            .color(ACCENT)
                            .size(12.0),
                    );
                }
                egui::CollapsingHeader::new(RichText::new("Webhooks (optional)").size(12.0))
                    .id_salt(("bot_webhooks", i))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                "One line per channel: channel_id=https://discord.com/api/webhooks/… \
                                 Lets characters post with their own names and avatars.",
                            )
                            .size(12.0)
                            .color(MUTED),
                        );
                        ui.add(
                            egui::TextEdit::multiline(&mut self.discord_webhooks[i])
                                .desired_rows(2)
                                .desired_width(f32::INFINITY)
                                .hint_text("123456789012345678=https://discord.com/api/webhooks/…"),
                        );
                        if let Err(e) = parse_webhooks(&self.discord_webhooks[i]) {
                            ui.label(RichText::new(e.to_string()).color(ACCENT).size(12.0));
                        }
                    });
            });
            ui.horizontal(|ui| {
                if running {
                    if ui.button("Disconnect").clicked() {
                        action = Some(BotAction::Disconnect(i));
                    }
                    if ui.button("Stop replies").clicked() {
                        self.discord_sessions[i].stop_replies();
                    }
                } else if Self::primary(ui, "Connect").clicked() {
                    action = Some(BotAction::Connect(i));
                }
                if !running
                    && ui
                        .button("Clear credentials")
                        .on_hover_text("Also deletes any saved in the keychain.")
                        .clicked()
                {
                    self.discord_tokens[i].clear();
                    self.discord_webhooks[i].clear();
                    forget = bot.remember;
                }
                if confirming {
                    ui.label(RichText::new("Remove this bot?").color(ACCENT));
                    if ui
                        .add_enabled(!running, egui::Button::new("Yes, remove"))
                        .clicked()
                    {
                        action = Some(BotAction::Remove(i));
                        ui.data_mut(|d| d.remove::<usize>(confirm_id));
                    }
                    if ui.button("Cancel").clicked() {
                        ui.data_mut(|d| d.remove::<usize>(confirm_id));
                    }
                } else if ui
                    .add_enabled(!running, egui::Button::new("Remove"))
                    .on_disabled_hover_text("Disconnect first")
                    .clicked()
                {
                    ui.data_mut(|d| d.insert_temp(confirm_id, i));
                }
            });
        });
        if forget {
            let name = self.config.discord_bots[i].name.clone();
            self.persist();
            if let Err(e) = forget_bot(&name) {
                self.error = Some(e.to_string());
            }
        }
        action
    }

    fn bot_action(&mut self, action: BotAction) {
        match action {
            BotAction::Disconnect(i) => {
                if let Some(session) = self.discord_sessions.get(i) {
                    session.disconnect();
                }
            }
            BotAction::Remove(i) => {
                let idle = self
                    .discord_sessions
                    .get(i)
                    .is_some_and(|s| !s.running.load(Ordering::Relaxed));
                if idle && i < self.config.discord_bots.len() {
                    let bot = self.config.discord_bots.remove(i);
                    if bot.remember
                        && let Err(e) = forget_bot(&bot.name)
                    {
                        self.error = Some(e.to_string());
                    }
                    self.discord_sessions.remove(i);
                    self.discord_tokens.remove(i);
                    self.discord_webhooks.remove(i);
                    self.persist();
                }
            }
            BotAction::Connect(i) => {
                let result = (|| -> anyhow::Result<()> {
                    let profile = self
                        .config
                        .discord_bots
                        .get(i)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Unknown bot"))?;
                    check_profile(&profile, &self.config.discord_bots)?;
                    // Blank fields fall back to what the keychain holds for this bot.
                    let (typed_token, typed_hooks) =
                        (&self.discord_tokens[i], &self.discord_webhooks[i]);
                    let saved = |typed: &str, key: String| {
                        if profile.remember && typed.trim().is_empty() {
                            secrets::get(&key).unwrap_or_default()
                        } else {
                            typed.to_owned()
                        }
                    };
                    let token = saved(typed_token, discord::token_key(&profile.name));
                    let hooks = saved(typed_hooks, discord::webhooks_key(&profile.name));
                    let webhooks = parse_webhooks(&hooks)?;
                    let (typed_token, typed_hooks) = (typed_token.clone(), typed_hooks.clone());
                    self.persist();
                    self.discord_sessions[i].connect(
                        token,
                        webhooks,
                        profile.clone(),
                        self.config.clone(),
                        self.library.clone(),
                    )?;
                    if profile.remember {
                        remember_bot(&profile.name, &typed_token, &typed_hooks).map_err(|e| {
                            anyhow::anyhow!("Connected, but the keychain failed: {e}")
                        })?;
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    self.error = Some(e.to_string());
                }
            }
        }
    }
}

/// Text box for a list of Discord IDs. The raw text lives in egui memory so
/// separators survive while typing; it resets when the list changes elsewhere.
fn id_list(ui: &mut egui::Ui, id: egui::Id, ids: &mut Vec<String>, hint: &str) {
    let mut text = ui
        .data(|d| d.get_temp::<String>(id))
        .filter(|t| parse_ids(t) == *ids)
        .unwrap_or_else(|| ids.join(", "));
    if ui
        .add(
            egui::TextEdit::multiline(&mut text)
                .desired_rows(1)
                .desired_width(f32::INFINITY)
                .hint_text(hint),
        )
        .changed()
    {
        *ids = parse_ids(&text);
    }
    ui.data_mut(|d| d.insert_temp(id, text));
}

/// Lorebook entries of character `i`: keys and text, with add and remove.
fn lore_editor(ui: &mut egui::Ui, i: usize, lore: &mut Vec<LoreEntry>) {
    let mut remove = None;
    for (j, entry) in lore.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            key_list(ui, egui::Id::new(("lore_keys", i, j)), &mut entry.keys);
            if ui.small_button("Remove").clicked() {
                remove = Some(j);
            }
        });
        ui.add(
            egui::TextEdit::multiline(&mut entry.text)
                .hint_text("Text added when a key is mentioned")
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .char_limit(MAX_LORE_TEXT),
        );
        ui.add_space(4.0);
    }
    if let Some(j) = remove {
        lore.remove(j);
    }
    if lore.len() < MAX_LORE && ui.button("+ Entry").clicked() {
        lore.push(LoreEntry::default());
    }
}

/// Memory notes of character `i`, one line each.
fn notes_editor(ui: &mut egui::Ui, i: usize, notes: &mut Vec<String>) {
    let mut remove = None;
    for (j, note) in notes.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(note)
                    .id_salt(("persona_note", i, j))
                    .hint_text("Something to remember")
                    .desired_width((ui.available_width() - 70.0).max(120.0))
                    .char_limit(MAX_NOTE),
            );
            if ui.small_button("Remove").clicked() {
                remove = Some(j);
            }
        });
    }
    if let Some(j) = remove {
        notes.remove(j);
    }
    if notes.len() < MAX_NOTES && ui.button("+ Note").clicked() {
        notes.push(String::new());
    }
}

/// Comma-separated lore keys. Like `id_list`, the raw text lives in egui memory so a trailing
/// comma survives while typing.
fn key_list(ui: &mut egui::Ui, id: egui::Id, keys: &mut Vec<String>) {
    let mut text = ui
        .data(|d| d.get_temp::<String>(id))
        .filter(|t| parse_keys(t) == *keys)
        .unwrap_or_else(|| keys.join(", "));
    if ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .hint_text("Keys, comma separated")
                .desired_width(260.0)
                .char_limit(600),
        )
        .on_hover_text("Matched as whole words, ignoring case.")
        .changed()
    {
        *keys = parse_keys(&text);
    }
    ui.data_mut(|d| d.insert_temp(id, text));
}

/// Splits lore keys on commas; trims, drops empties and case-insensitive duplicates, and keeps
/// at most 20 keys of 64 characters.
fn parse_keys(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in text.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        let key: String = key.chars().take(64).collect();
        if out.len() < 20 && !out.iter().any(|o| o.to_lowercase() == key.to_lowercase()) {
            out.push(key);
        }
    }
    out
}

/// Saves a bot's typed token and webhook lines in the keychain (blank fields are left as they
/// are, since blank means "use the saved value").
fn remember_bot(name: &str, token: &str, webhooks: &str) -> anyhow::Result<()> {
    if !token.trim().is_empty() {
        secrets::set(&discord::token_key(name), token.trim())?;
    }
    if !webhooks.trim().is_empty() {
        secrets::set(&discord::webhooks_key(name), webhooks.trim())?;
    }
    Ok(())
}

fn forget_bot(name: &str) -> anyhow::Result<()> {
    secrets::delete(&discord::token_key(name))?;
    secrets::delete(&discord::webhooks_key(name))
}

fn scope_label(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Channel => "Per channel",
        MemoryScope::Server => "Per server",
        MemoryScope::User => "Per user",
    }
}

/// Splits on commas, whitespace and newlines; drops empties and duplicates.
fn parse_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in text.split(|c: char| c == ',' || c.is_whitespace()) {
        if !id.is_empty() && !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

fn is_snowflake(id: &str) -> bool {
    (15..=21).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Parses "channel_id=webhook_url" lines; blank lines and `#` comments are skipped.
fn parse_webhooks(text: &str) -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (channel, url) = line
            .split_once('=')
            .map(|(c, u)| (c.trim(), u.trim()))
            .ok_or_else(|| anyhow::anyhow!("Webhook line {}: use channel_id=URL", n + 1))?;
        anyhow::ensure!(
            is_snowflake(channel),
            "Webhook line {}: channel ID must be numeric",
            n + 1
        );
        anyhow::ensure!(
            [
                "https://discord.com/api/webhooks/",
                "https://discordapp.com/api/webhooks/"
            ]
            .iter()
            .any(|p| url.starts_with(p))
                && url.len() <= 300,
            "Webhook line {}: URL must be a Discord webhook",
            n + 1
        );
        anyhow::ensure!(
            out.insert(channel.to_string(), url.to_string()).is_none(),
            "Webhook line {}: channel listed twice",
            n + 1
        );
    }
    Ok(out)
}

/// Checks a bot before connecting: names must map to distinct memory folders.
fn check_profile(profile: &BotProfile, all: &[BotProfile]) -> anyhow::Result<()> {
    anyhow::ensure!(!profile.name.trim().is_empty(), "Give the bot a name");
    let folder = discord::sanitize(&profile.name);
    anyhow::ensure!(
        all.iter()
            .filter(|b| discord::sanitize(&b.name) == folder)
            .count()
            == 1,
        "Bot names must differ in letters or digits"
    );
    anyhow::ensure!(
        !profile.channels.is_empty(),
        "Enter at least one channel ID"
    );
    anyhow::ensure!(
        profile
            .channels
            .iter()
            .chain(&profile.guilds)
            .all(|id| is_snowflake(id)),
        "Channel and server IDs must be numeric Discord IDs"
    );
    anyhow::ensure!(!profile.prefix.trim().is_empty(), "Enter a command prefix");
    Ok(())
}

fn taken_names(personas: &[Persona]) -> HashSet<String> {
    personas
        .iter()
        .map(|p| p.name.trim().to_lowercase())
        .collect()
}

/// Returns `base`, or `base (2)`, `base (3)`… so it is not in `taken`
/// (lowercase) and stays within 64 characters.
fn unique_name(base: &str, taken: &HashSet<String>) -> String {
    let base = match base.trim() {
        "" => "Character",
        b => b,
    };
    let fit = |suffix: &str| -> String {
        let keep = 64 - suffix.chars().count();
        let head: String = base.chars().take(keep).collect();
        format!("{}{suffix}", head.trim_end())
    };
    let first = fit("");
    if !taken.contains(&first.to_lowercase()) {
        return first;
    }
    (2..)
        .map(|n| fit(&format!(" ({n})")))
        .find(|name| !taken.contains(&name.to_lowercase()))
        .expect("unbounded range")
}

/// Appends `incoming` to `existing`, renaming any name clash.
fn merge_personas(existing: &[Persona], incoming: Vec<Persona>) -> Vec<Persona> {
    let mut taken = taken_names(existing);
    let mut out = existing.to_vec();
    for mut p in incoming {
        p.name = unique_name(&p.name, &taken);
        taken.insert(p.name.to_lowercase());
        out.push(p);
    }
    out
}

fn import_personas(
    path: &std::path::Path,
    existing: &[Persona],
) -> anyhow::Result<(Vec<Persona>, usize)> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= MAX_CHARACTER_FILE,
        "Character file exceeds 256 KB"
    );
    let incoming = match serde_json::from_slice::<PersonaFile>(&std::fs::read(path)?)? {
        PersonaFile::Many(list) => list,
        PersonaFile::One(p) => vec![p],
    };
    anyhow::ensure!(!incoming.is_empty(), "The file has no characters");
    let added = incoming.len();
    let merged = merge_personas(existing, incoming);
    crate::core::personas::validate(&merged)?;
    Ok((merged, added))
}

fn write_json(path: &std::path::Path, value: &impl Serialize) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

/// A safe file name stem for a character export.
fn file_stem(name: &str) -> String {
    let stem: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "character".into()
    } else {
        stem.chars().take(64).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> Persona {
        Persona {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn merge_renames_clashes() {
        let merged = merge_personas(
            &[named("Ada"), named("Bob")],
            vec![named("ada"), named("Ada"), named("Cy")],
        );
        let names: Vec<_> = merged.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Ada", "Bob", "ada (2)", "Ada (3)", "Cy"]);
        assert!(crate::core::personas::validate(&merged).is_ok());
    }

    #[test]
    fn unique_name_stays_within_limit() {
        let long = "x".repeat(64);
        let taken = HashSet::from([long.clone()]);
        let name = unique_name(&long, &taken);
        assert_eq!(name.chars().count(), 64);
        assert!(name.ends_with(" (2)"));
        assert_eq!(unique_name("  ", &HashSet::new()), "Character");
    }

    #[test]
    fn parses_lore_keys() {
        assert_eq!(
            parse_keys(" Dragon, new york ,,dragon, "),
            ["Dragon", "new york"]
        );
        assert!(parse_keys(" , ").is_empty());
        let many: Vec<String> = (0..30).map(|n| n.to_string()).collect();
        assert_eq!(parse_keys(&many.join(",")).len(), 20);
        assert_eq!(parse_keys(&"k".repeat(80))[0].chars().count(), 64);
    }

    #[test]
    fn parses_id_lists() {
        assert_eq!(parse_ids("1, 2\n3,,2 "), ["1", "2", "3"]);
        assert!(parse_ids(" ,\n").is_empty());
        assert!(is_snowflake("123456789012345678"));
        assert!(!is_snowflake("12ab"));
    }

    #[test]
    fn parses_webhook_lines() {
        let hooks = parse_webhooks(
            "# comment\n123456789012345678 = https://discord.com/api/webhooks/1/abc\n\n",
        )
        .unwrap();
        assert_eq!(
            hooks["123456789012345678"],
            "https://discord.com/api/webhooks/1/abc"
        );
        assert!(parse_webhooks("nope").is_err());
        assert!(parse_webhooks("123456789012345678=https://evil.example/x").is_err());
        assert!(parse_webhooks("1=https://discord.com/api/webhooks/1/a").is_err());
    }

    #[test]
    fn checks_bot_profiles() {
        let bot = BotProfile {
            channels: vec!["123456789012345678".into()],
            ..Default::default()
        };
        assert!(check_profile(&bot, std::slice::from_ref(&bot)).is_ok());
        assert!(check_profile(&bot, &[bot.clone(), bot.clone()]).is_err());
        let similar = BotProfile {
            name: "bot_1".into(),
            ..bot.clone()
        };
        assert!(check_profile(&bot, &[bot.clone(), similar]).is_err());
        let empty = BotProfile::default();
        assert!(check_profile(&empty, std::slice::from_ref(&empty)).is_err());
    }

    #[test]
    fn character_file_accepts_one_or_many() {
        let one: PersonaFile = serde_json::from_str(r#"{"name":"Ada"}"#).unwrap();
        assert!(matches!(one, PersonaFile::One(p) if p.name == "Ada"));
        let many: PersonaFile = serde_json::from_str(r#"[{"name":"Ada"}]"#).unwrap();
        assert!(matches!(many, PersonaFile::Many(v) if v.len() == 1));
        assert_eq!(file_stem("Ada / Lovelace"), "Ada---Lovelace");
        assert_eq!(file_stem("///"), "character");
    }
}

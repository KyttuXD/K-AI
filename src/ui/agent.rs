//! Settings → Agent: the editable prompt texts, suggestions, SOUL and a read-only preview of
//! the assembled system prompt. Defaults live in assets/prompts; edits are saved to the data
//! folder through `prompts::save`.
use super::icons;
use super::*;
use crate::core::prompts::{self, PromptId, PromptParts, Suggestion};

/// Edits are written this long after the last keystroke.
const DEBOUNCE: Duration = Duration::from_millis(700);
const MAX_CHARS: usize = 8000;
const COUNT: usize = PromptId::ALL.len();

/// Rough token count for a text of `chars` characters.
fn tokens(chars: usize) -> usize {
    (chars as f32 / 3.5).ceil() as usize
}

/// Editor state for Settings → Agent. Texts are loaded on first use.
#[derive(Default)]
pub struct AgentState {
    loaded: bool,
    texts: Vec<String>,
    custom: Vec<bool>,
    dirty: Vec<Option<Instant>>,
    chat: Vec<Suggestion>,
    code: Vec<Suggestion>,
    sugg_dirty: Option<Instant>,
    /// 0 Chat, 1 Code, 2 Discord tool host.
    preview_mode: u8,
    preview_character: bool,
    preview_sources: bool,
    /// Bumped after every save or reset, so the preview is rebuilt.
    saves: u32,
    cache: Option<Cache>,
}

struct Cache {
    key: (bool, bool, u8, bool, bool, u32),
    prompt: String,
    soul_chars: usize,
}

impl AgentState {
    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        self.texts = PromptId::ALL.iter().map(|id| prompts::text(*id)).collect();
        self.custom = PromptId::ALL
            .iter()
            .map(|id| prompts::is_custom(*id))
            .collect();
        self.dirty = vec![None; COUNT];
        self.chat = prompts::suggestions(false);
        self.code = prompts::suggestions(true);
    }
    fn any_dirty(&self) -> bool {
        self.sugg_dirty.is_some() || self.dirty.iter().any(Option::is_some)
    }
}

fn copy_rows(rows: &[Suggestion]) -> Vec<Suggestion> {
    rows.iter()
        .filter(|s| !s.prompt.trim().is_empty())
        .map(|s| Suggestion {
            title: s.title.trim().to_owned(),
            prompt: s.prompt.clone(),
        })
        .collect()
}

impl Studio {
    pub(super) fn agent_settings(&mut self, ui: &mut egui::Ui) {
        self.agent.load();
        self.flush_agent(false);
        self.refresh_agent_preview();
        self.soul_card(ui);
        ui.add_space(8.0);
        self.prompts_card(ui);
        ui.add_space(8.0);
        self.suggestions_card(ui);
        ui.add_space(8.0);
        self.preview_card(ui);
        if self.agent.any_dirty() {
            ui.ctx().request_repaint_after(DEBOUNCE);
        }
    }

    /// Writes pending edits: all of them when `force`, else those idle for the debounce time.
    /// Also call it with `true` when leaving Settings and on exit.
    pub(super) fn flush_agent(&mut self, force: bool) {
        if !self.agent.loaded {
            return;
        }
        let now = Instant::now();
        for i in 0..COUNT {
            let Some(t) = self.agent.dirty[i] else {
                continue;
            };
            if !force && now.duration_since(t) < DEBOUNCE {
                continue;
            }
            self.agent.dirty[i] = None;
            let id = PromptId::ALL[i];
            // Text equal to the default is not worth a file of its own.
            let result = if self.agent.texts[i] == prompts::default_text(id) {
                prompts::reset(id)
            } else {
                prompts::save(id, &self.agent.texts[i])
            };
            match result {
                Ok(()) => {
                    self.agent.custom[i] = prompts::is_custom(id);
                    self.agent.saves += 1;
                }
                Err(e) => self.error_toast(format!("Could not save {}: {e:#}", id.label())),
            }
        }
        if let Some(t) = self.agent.sugg_dirty
            && (force || now.duration_since(t) >= DEBOUNCE)
        {
            self.agent.sugg_dirty = None;
            let chat = copy_rows(&self.agent.chat);
            let code = copy_rows(&self.agent.code);
            match prompts::save_suggestions(&chat, &code) {
                Ok(()) => self.agent.saves += 1,
                Err(e) => self.error_toast(format!("Could not save suggestions: {e:#}")),
            }
        }
    }

    /// Rebuilds the cached preview and SOUL size when something it depends on changed.
    fn refresh_agent_preview(&mut self) {
        let a = &self.agent;
        let key = (
            self.config.soul_enabled,
            self.config.soul_when_no_character,
            a.preview_mode,
            a.preview_character,
            a.preview_sources,
            a.saves,
        );
        if a.cache.as_ref().is_some_and(|c| c.key == key) {
            return;
        }
        let mode = a.preview_mode;
        // The project SOUL.md is only sent in Code mode without a host.
        let full_soul = if mode == 1 {
            prompts::soul_text(&self.config.workspace, &self.config)
        } else {
            prompts::soul_text_global(&self.config)
        };
        let soul_chars = full_soul.as_ref().map_or(0, |s| s.chars().count());
        let character = a.preview_character;
        let soul = if character || self.config.soul_when_no_character {
            full_soul
        } else {
            None
        };
        let workspace = self.config.workspace.clone();
        let parts = PromptParts {
            model: "sample-model",
            workspace: (mode == 1).then_some(workspace.as_path()),
            coding: mode == 1,
            host: mode == 2,
            docs_or_web: a.preview_sources,
            character: character.then_some(("Sample name", "Sample notes", "Sample memory")),
            soul,
        };
        let prompt = prompts::assemble(&parts);
        self.agent.cache = Some(Cache {
            key,
            prompt,
            soul_chars,
        });
    }

    /// One multiline editor for prompt number `i` with its size and Reset.
    fn prompt_editor(&mut self, ui: &mut egui::Ui, i: usize, rows: usize) {
        let p = pal(ui.ctx());
        let id = PromptId::ALL[i];
        let response = ui.add(
            egui::TextEdit::multiline(&mut self.agent.texts[i])
                .font(egui::TextStyle::Monospace)
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .char_limit(MAX_CHARS),
        );
        if response.changed() {
            self.agent.dirty[i] = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            let n = self.agent.texts[i].chars().count();
            ui.label(
                RichText::new(format!("{n} chars, ~{} tokens", tokens(n)))
                    .size(11.5)
                    .color(p.muted),
            );
            if self.agent.custom[i]
                && ui
                    .small_button(icons::label(icons::UNDO, "Reset to default"))
                    .on_hover_text("Delete your copy and use the built-in text again.")
                    .clicked()
            {
                self.agent.dirty[i] = None;
                match prompts::reset(id) {
                    Ok(()) => {
                        self.agent.texts[i] = prompts::default_text(id).to_owned();
                        self.agent.custom[i] = false;
                        self.agent.saves += 1;
                    }
                    Err(e) => self.error_toast(format!("Could not reset: {e:#}")),
                }
            }
        });
    }

    fn soul_card(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let soul = PromptId::ALL
            .iter()
            .position(|x| *x == PromptId::Soul)
            .unwrap_or(0);
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "SOUL");
            ui.checkbox(&mut self.config.soul_enabled, "Enable SOUL")
                .on_hover_text(
                    "Core rules that always apply and override the character. A SOUL.md in the workspace adds project rules.",
                );
            ui.add_enabled_ui(self.config.soul_enabled, |ui| {
                ui.checkbox(
                    &mut self.config.soul_when_no_character,
                    "Also apply with no character",
                )
                .on_hover_text("Otherwise SOUL is only used while a character is active.");
                self.prompt_editor(ui, soul, 8);
                let chars = self.agent.cache.as_ref().map_or(0, |c| c.soul_chars);
                ui.label(
                    RichText::new(format!("SOUL as sent: ~{} tokens", tokens(chars)))
                        .size(11.5)
                        .color(p.muted),
                )
                .on_hover_text("Estimate (characters / 3.5), including project rules from SOUL.md. Capped at 4000 characters.");
            });
        });
    }

    fn prompts_card(&mut self, ui: &mut egui::Ui) {
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "PROMPTS");
            for i in 0..COUNT {
                let id = PromptId::ALL[i];
                if id == PromptId::Soul {
                    continue;
                }
                let title = if self.agent.custom[i] {
                    format!("{} (edited)", id.label())
                } else {
                    id.label().to_owned()
                };
                egui::CollapsingHeader::new(title)
                    .id_salt(("agent_prompt", i))
                    .show(ui, |ui| {
                        self.prompt_editor(ui, i, 8);
                    })
                    .header_response
                    .on_hover_text(format!(
                        "File: prompts/{}. Placeholders like {{{{model}}}} are filled in when the prompt is built.",
                        id.file()
                    ));
            }
        });
    }

    fn suggestions_card(&mut self, ui: &mut egui::Ui) {
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "SUGGESTIONS");
            for code in [false, true] {
                self.suggestion_rows(ui, code);
            }
            if ui
                .small_button(icons::label(icons::UNDO, "Reset suggestions"))
                .on_hover_text("Delete your suggestions file and use the built-in ones.")
                .clicked()
            {
                self.agent.sugg_dirty = None;
                match prompts::reset_suggestions() {
                    Ok(()) => {
                        self.agent.chat = prompts::suggestions(false);
                        self.agent.code = prompts::suggestions(true);
                        self.agent.saves += 1;
                    }
                    Err(e) => self.error_toast(format!("Could not reset: {e:#}")),
                }
            }
        });
    }

    fn suggestion_rows(&mut self, ui: &mut egui::Ui, code: bool) {
        let p = pal(ui.ctx());
        let name = if code { "Code" } else { "Chat" };
        let mut changed = false;
        let mut remove = None;
        let mut add = false;
        egui::CollapsingHeader::new(name)
            .id_salt(("agent_suggestions", code))
            .show(ui, |ui| {
                let rows = if code {
                    &mut self.agent.code
                } else {
                    &mut self.agent.chat
                };
                for (n, row) in rows.iter_mut().enumerate() {
                    ui.horizontal_top(|ui| {
                        changed |= ui
                            .add(
                                egui::TextEdit::singleline(&mut row.title)
                                    .hint_text("Title")
                                    .desired_width(130.0),
                            )
                            .changed();
                        let width = (ui.available_width() - 36.0).max(80.0);
                        changed |= ui
                            .add(
                                egui::TextEdit::multiline(&mut row.prompt)
                                    .hint_text("Prompt")
                                    .desired_rows(1)
                                    .desired_width(width),
                            )
                            .changed();
                        if ui
                            .small_button(icons::TRASH)
                            .on_hover_text("Remove")
                            .clicked()
                        {
                            remove = Some(n);
                        }
                    });
                }
                if rows.is_empty() {
                    ui.label(RichText::new("No suggestions.").size(11.5).color(p.muted));
                }
                if ui
                    .small_button(icons::label(icons::PLUS, "Add"))
                    .on_hover_text("Rows without a prompt are not saved.")
                    .clicked()
                {
                    add = true;
                }
            });
        if add {
            let rows = if code {
                &mut self.agent.code
            } else {
                &mut self.agent.chat
            };
            rows.push(Suggestion {
                title: String::new(),
                prompt: String::new(),
            });
            changed = true;
        }
        if let Some(n) = remove {
            let rows = if code {
                &mut self.agent.code
            } else {
                &mut self.agent.chat
            };
            let gone = rows.remove(n);
            changed = true;
            self.undo_toast(
                "Suggestion removed",
                Box::new(move |s: &mut Studio| {
                    let rows = if code {
                        &mut s.agent.code
                    } else {
                        &mut s.agent.chat
                    };
                    let at = n.min(rows.len());
                    rows.insert(at, gone);
                    s.agent.sugg_dirty = Some(Instant::now());
                }),
                Box::new(|| {}),
            );
        }
        if changed {
            self.agent.sugg_dirty = Some(Instant::now());
        }
    }

    fn preview_card(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        Self::card(ui, |ui| {
            Self::eyebrow(ui, "PREVIEW");
            ui.horizontal_wrapped(|ui| {
                for (mode, name) in [(0u8, "Chat"), (1, "Code"), (2, "Discord")] {
                    if ui
                        .selectable_label(self.agent.preview_mode == mode, name)
                        .clicked()
                    {
                        self.agent.preview_mode = mode;
                    }
                }
                ui.checkbox(&mut self.agent.preview_character, "With a character")
                    .on_hover_text("Sample values are used for the character.");
                if self.agent.preview_mode == 0 {
                    ui.checkbox(&mut self.agent.preview_sources, "Docs or web")
                        .on_hover_text(
                            "Chat only adds the sources rules when documents or web search are on.",
                        );
                }
            });
            let Some(cache) = self.agent.cache.as_ref() else {
                return;
            };
            let n = cache.prompt.chars().count();
            ui.label(
                RichText::new(format!("Full prompt: {n} chars, ~{} tokens", tokens(n)))
                    .size(11.5)
                    .color(p.muted),
            )
            .on_hover_text("Read-only, built from the saved texts with sample values. Estimate: characters / 3.5.");
            let mut shown: &str = &cache.prompt;
            egui::ScrollArea::vertical()
                .id_salt("agent_preview")
                .max_height(280.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut shown)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });
        });
    }
}

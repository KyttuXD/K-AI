//! Models page: installed GGUF files with hot-swap, per-model presets,
//! recommended settings, Hugging Face downloads, the embedding server and a
//! live performance readout. Network and disk work runs on worker threads.
use super::*;
use crate::core::{hub, llama::ModelPreset};
use std::{fs, ops::RangeInclusive, path::Path};

/// A worker thread's result, filled once when it finishes.
type Task<T> = Arc<Mutex<Option<Result<T, String>>>>;

fn spawn_task<T: Send + 'static>(
    ctx: &egui::Context,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Task<T> {
    let slot: Task<T> = Default::default();
    let (out, ctx) = (slot.clone(), ctx.clone());
    std::thread::spawn(move || {
        let result = work().map_err(|e| format!("{e:#}"));
        *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        ctx.request_repaint();
    });
    slot
}

/// Takes a finished task's result and clears it; pending tasks stay.
fn take<T>(task: &mut Option<Task<T>>) -> Option<Result<T, String>> {
    let done = task
        .as_ref()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if done.is_some() {
        *task = None;
    }
    done
}

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Chat { use_after: bool },
    Embed,
}

struct Download {
    name: String,
    purpose: Purpose,
    progress: Arc<Mutex<setup::SetupProgress>>,
    cancel: Arc<AtomicBool>,
    task: Task<PathBuf>,
}

/// What a click on the page asks for, applied after the page is drawn.
enum Action {
    Use(PathBuf),
    Delete(PathBuf),
    Download {
        file: String,
        url: String,
        size: u64,
        sha256: Option<String>,
        purpose: Purpose,
    },
    Search,
    Files(String),
    EmbedStart,
    EmbedStop,
    AddFile,
}

#[derive(Default)]
pub struct ModelsState {
    installed: Vec<(PathBuf, u64)>,
    installed_at: Option<Instant>,
    confirm_delete: Option<PathBuf>,
    recommended: Option<setup::RecommendedSettings>,
    /// File name whose preset is edited; empty = the active model.
    preset_for: String,
    query: String,
    searching: Option<Task<Vec<hub::HubModel>>>,
    results: Vec<hub::HubModel>,
    repo: String,
    listing: Option<Task<Vec<hub::HubFile>>>,
    files: Vec<hub::HubFile>,
    use_after: bool,
    download: Option<Download>,
    embed_start: Option<Task<u16>>,
    /// Cached by the 5 s refresh so cards never touch the disk per frame.
    active_size: Option<u64>,
    recommended_installed: bool,
    embed_have: bool,
    note: String,
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// Cuts `text` to `max` characters, ending with "…" when shortened.
fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.into();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One optional preset value: unchecked means the global setting applies.
fn opt_field<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    initial: T,
    global: String,
    range: RangeInclusive<T>,
    speed: f64,
) {
    let muted = pal(ui.ctx()).muted;
    let mut on = value.is_some();
    ui.checkbox(&mut on, label)
        .on_hover_text("Off: use the global setting");
    if on {
        let mut v = value.unwrap_or(initial);
        ui.add(egui::DragValue::new(&mut v).range(range).speed(speed));
        *value = Some(v);
    } else {
        *value = None;
        ui.label(RichText::new(global).size(12.0).color(muted));
    }
    ui.end_row();
}

impl Studio {
    /// Collects finished background work. Cheap; safe to call every frame.
    pub(super) fn models_poll(&mut self) {
        let finished = self
            .models_state
            .download
            .as_ref()
            .and_then(|d| d.task.lock().unwrap_or_else(|e| e.into_inner()).take());
        if let Some(result) = finished
            && let Some(d) = self.models_state.download.take()
        {
            self.models_state.installed_at = None;
            match result {
                Ok(path) => match d.purpose {
                    Purpose::Embed => {
                        self.config.embedding_gguf = path;
                        self.persist();
                    }
                    Purpose::Chat { use_after: true } => self.use_model(path),
                    Purpose::Chat { .. } => {
                        self.models_state.note = format!("Downloaded {}", d.name)
                    }
                },
                Err(_) if d.cancel.load(Ordering::Relaxed) => {
                    self.models_state.note = "Download cancelled".into()
                }
                Err(e) => self.error = Some(format!("Download failed: {e}")),
            }
        }
        match take(&mut self.models_state.searching) {
            Some(Ok(models)) => {
                self.models_state.note = if models.is_empty() {
                    "No GGUF models found".into()
                } else {
                    String::new()
                };
                self.models_state.results = models;
            }
            Some(Err(e)) => self.error = Some(format!("Search failed: {e}")),
            None => {}
        }
        match take(&mut self.models_state.listing) {
            Some(Ok(files)) => self.models_state.files = files,
            Some(Err(e)) => self.error = Some(format!("Could not list files: {e}")),
            None => {}
        }
        if let Some(Err(e)) = take(&mut self.models_state.embed_start) {
            self.error = Some(format!("Embedding server: {e}"));
        }
    }

    /// Switches the chat model: hot-swaps a running server, else starts one.
    fn use_model(&mut self, path: PathBuf) {
        if self.llama_busy || self.worker.is_some() || self.setup.is_some() {
            self.models_state.note =
                format!("Downloaded {}; press Use to switch", file_name(&path));
            return;
        }
        self.config.gguf_model = path;
        if !self.llama.running() {
            self.start_llama();
            return;
        }
        self.llama_busy = true;
        self.llama_crash = None;
        self.crash_reported = false;
        self.connection = "Loading model…".into();
        let (runtime, config) = (self.llama.clone(), self.config.clone());
        self.background(move || {
            SystemEvent::Llama(runtime.restart_with(&config).map_err(|e| format!("{e:#}")))
        });
    }

    pub(super) fn models_page(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.models_poll();
        let s = &mut self.models_state;
        if s.installed_at
            .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
        {
            s.installed = hub::installed();
            s.installed_at = Some(Instant::now());
            // File checks happen here, not per frame: these paths may be on slow drives.
            s.active_size = fs::metadata(&self.config.gguf_model)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len());
            let rec = s.recommended.get_or_insert_with(setup::recommend_settings);
            s.recommended_installed = setup::models_dir().join(rec.model.file).is_file();
            let default = setup::models_dir().join(setup::EMBED_MODEL.file);
            if !self.config.embedding_gguf.is_file() && default.is_file() {
                self.config.embedding_gguf = default;
            }
            s.embed_have = self.config.embedding_gguf.is_file();
        }
        let busy = s.download.is_some() || s.searching.is_some() || s.listing.is_some();
        if busy || s.embed_start.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        Self::page_title(ui, "Models");
        let mut actions = vec![];
        egui::ScrollArea::vertical()
            .id_salt("models_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(820.0));
                self.download_strip(ui);
                self.metrics_card(ui);
                ui.add_space(10.0);
                self.installed_card(ui, &mut actions);
                ui.add_space(10.0);
                self.preset_card(ui);
                ui.add_space(10.0);
                self.recommended_card(ui, &mut actions);
                ui.add_space(10.0);
                self.hub_card(ui, &mut actions);
                ui.add_space(10.0);
                self.embed_card(ui, &mut actions);
            });
        for action in actions {
            self.apply_action(&ctx, action);
        }
    }

    fn apply_action(&mut self, ctx: &egui::Context, action: Action) {
        let s = &mut self.models_state;
        match action {
            Action::Use(path) => self.use_model(path),
            Action::Delete(path) => {
                s.confirm_delete = None;
                s.installed_at = None;
                match hub::delete(&path) {
                    Ok(()) => {
                        self.config.model_presets.remove(&file_name(&path));
                        if self.config.gguf_model == path {
                            self.config.gguf_model = PathBuf::new();
                        }
                        if self.config.embedding_gguf == path {
                            self.config.embedding_gguf = PathBuf::new();
                        }
                        self.persist();
                    }
                    Err(e) => self.error = Some(format!("Could not delete: {e:#}")),
                }
            }
            Action::Download {
                file,
                url,
                size,
                sha256,
                purpose,
            } => {
                if s.download.is_some() {
                    return;
                }
                let name = file_name(Path::new(&file));
                let dest = setup::models_dir().join(&name);
                let progress: Arc<Mutex<setup::SetupProgress>> = Default::default();
                let cancel = Arc::new(AtomicBool::new(false));
                let (pr, ca) = (progress.clone(), cancel.clone());
                let task = spawn_task(ctx, move || {
                    fs::create_dir_all(setup::models_dir())?;
                    setup::download_verified(&url, size, sha256.as_deref(), &dest, &pr, &ca)
                });
                s.download = Some(Download {
                    name,
                    purpose,
                    progress,
                    cancel,
                    task,
                });
            }
            Action::Search => {
                let query = s.query.trim().to_string();
                if !query.is_empty() && s.searching.is_none() {
                    s.repo.clear();
                    s.files.clear();
                    s.searching = Some(spawn_task(ctx, move || hub::search(&query)));
                }
            }
            Action::Files(repo) => {
                s.files.clear();
                if s.repo == repo {
                    s.repo.clear();
                } else {
                    s.repo = repo.clone();
                    s.listing = Some(spawn_task(ctx, move || hub::files(&repo)));
                }
            }
            Action::EmbedStart => {
                if s.embed_start.is_none() {
                    let (embed, config) = (self.embed_runtime.clone(), self.config.clone());
                    s.embed_start = Some(spawn_task(ctx, move || embed.start(&config)));
                }
            }
            Action::EmbedStop => self.embed_runtime.stop(),
            Action::AddFile => self.choose_model_file(),
        }
    }

    fn download_strip(&mut self, ui: &mut egui::Ui) {
        let note = self.models_state.note.clone();
        if !note.is_empty() {
            let mut dismiss = false;
            Self::banner(ui, false, |ui| {
                ui.horizontal(|ui| {
                    ui.label(note);
                    dismiss = ui.small_button("Dismiss").clicked();
                });
            });
            if dismiss {
                self.models_state.note.clear();
            }
        }
        let Some(d) = &self.models_state.download else {
            return;
        };
        let p = d.progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut cancel = false;
        Self::banner(ui, false, |ui| {
            ui.horizontal(|ui| {
                busy(ui);
                ui.label(format!("Downloading {}", short(&d.name, 60)));
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
        });
        if cancel {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn metrics_card(&self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let running = self.llama.running();
        let fields = self
            .metrics
            .as_ref()
            .map(|m| (m.ram_mb, m.ctx_used, m.ctx_total, m.tokens_per_sec));
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "PERFORMANCE");
            if !running {
                ui.label(RichText::new("Model not running").color(p.muted));
                return;
            }
            let Some((ram, used, total, tps)) = fields else {
                ui.horizontal(|ui| {
                    busy(ui);
                    ui.label(RichText::new("Reading…").color(p.muted));
                });
                return;
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{ram} MB RAM")).strong());
                ui.label(RichText::new("·").color(p.muted));
                ui.label(
                    RichText::new(match tps {
                        Some(t) => format!("{t:.1} tokens/s"),
                        None => "– tokens/s".into(),
                    })
                    .strong(),
                )
                .on_hover_text("Generation speed of the last reply");
            });
            if total > 0 {
                let fraction = (used as f32 / total as f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .text(format!("Context {used} / {total} tokens")),
                );
            }
        });
    }

    fn installed_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let p = pal(ui.ctx());
        let running = self.llama.running();
        let can_swap = self.worker.is_none() && !self.llama_busy && self.setup.is_none();
        let active = self.config.gguf_model.clone();
        let models_dir = setup::models_dir();
        let mut rows = self.models_state.installed.clone();
        if let Some(size) = self.models_state.active_size
            && !rows.iter().any(|(path, _)| *path == active)
        {
            rows.insert(0, (active.clone(), size));
        }
        let s = &mut self.models_state;
        let embed = &self.config.embedding_gguf;
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                Self::eyebrow(ui, "INSTALLED");
                if self.llama_busy {
                    busy(ui);
                }
            });
            if rows.is_empty() {
                ui.label(RichText::new("No models yet").color(p.muted))
                    .on_hover_text(models_dir.display().to_string());
            }
            egui::Grid::new("installed_models")
                .num_columns(3)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    for (path, size) in &rows {
                        let name = file_name(path);
                        let is_active = *path == active;
                        let is_embed = path == embed || name == setup::EMBED_MODEL.file;
                        let text = if is_active {
                            RichText::new(format!("● {}", short(&name, 52)))
                                .strong()
                                .color(p.accent)
                        } else {
                            RichText::new(short(&name, 54))
                        };
                        ui.label(text).on_hover_text(path.display().to_string());
                        ui.label(RichText::new(format_size(*size)).color(p.muted));
                        ui.horizontal(|ui| {
                            if is_embed {
                                ui.label(RichText::new("Embedding").size(12.0).color(p.muted));
                            } else if is_active && running {
                                ui.label(RichText::new("In use").size(12.0).color(p.accent));
                            } else if ui
                                .add_enabled(can_swap, egui::Button::new("Use"))
                                .on_hover_text("Load this model")
                                .on_disabled_hover_text("Wait for the current task to finish")
                                .clicked()
                            {
                                actions.push(Action::Use(path.clone()));
                            }
                            let deletable =
                                path.starts_with(&models_dir) && !(is_active && running);
                            if !deletable {
                                return;
                            }
                            if s.confirm_delete.as_ref() == Some(path) {
                                ui.label(RichText::new("Delete?").color(p.danger_text));
                                if ui.small_button("Yes").clicked() {
                                    actions.push(Action::Delete(path.clone()));
                                }
                                if ui.small_button("No").clicked() {
                                    s.confirm_delete = None;
                                }
                            } else if ui
                                .small_button("Delete")
                                .on_hover_text("Remove the file from disk")
                                .clicked()
                            {
                                s.confirm_delete = Some(path.clone());
                            }
                        });
                        ui.end_row();
                    }
                });
            if ui
                .small_button("Add file…")
                .on_hover_text("Choose a .gguf file from disk")
                .clicked()
            {
                actions.push(Action::AddFile);
            }
        });
    }

    fn preset_card(&mut self, ui: &mut egui::Ui) {
        let p = pal(ui.ctx());
        let active = file_name(&self.config.gguf_model);
        let names: Vec<String> = self
            .models_state
            .installed
            .iter()
            .map(|(path, _)| file_name(path))
            .filter(|n| n != setup::EMBED_MODEL.file)
            .chain((!active.is_empty()).then(|| active.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let s = &mut self.models_state;
        if s.preset_for.is_empty() || !names.contains(&s.preset_for) {
            s.preset_for = active.clone();
        }
        let key = s.preset_for.clone();
        let c = &mut self.config;
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "MODEL PRESET");
            if key.is_empty() {
                ui.label(RichText::new("No model selected").color(p.muted));
            } else {
                egui::ComboBox::from_id_salt("preset_model")
                    .selected_text(short(&key, 48))
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for name in &names {
                            ui.selectable_value(&mut s.preset_for, name.clone(), short(name, 60));
                        }
                    })
                    .response
                    .on_hover_text("Settings used when this model runs; unchecked = global");
                let mut preset = c.model_presets.get(&key).cloned().unwrap_or_default();
                egui::Grid::new("preset_grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        opt_field(
                            ui,
                            "Context",
                            &mut preset.context_tokens,
                            c.context_tokens,
                            format!("{} tokens", c.context_tokens),
                            512..=262_144,
                            64.0,
                        );
                        let threads = match c.llama_threads {
                            0 => "auto".to_string(),
                            n => n.to_string(),
                        };
                        opt_field(
                            ui,
                            "Threads",
                            &mut preset.threads,
                            c.llama_threads.max(1),
                            threads,
                            1..=128,
                            0.2,
                        );
                        let gpu = match c.llama_gpu_layers {
                            None => "auto".to_string(),
                            Some(n) => format!("{n} layers"),
                        };
                        opt_field(
                            ui,
                            "GPU layers",
                            &mut preset.gpu_layers,
                            c.llama_gpu_layers.unwrap_or(0),
                            gpu,
                            0..=999,
                            0.5,
                        );
                        opt_field(
                            ui,
                            "Temperature",
                            &mut preset.temperature,
                            c.temperature,
                            format!("{:.2}", c.temperature),
                            0.0..=2.0,
                            0.01,
                        );
                        opt_field(
                            ui,
                            "Top-p",
                            &mut preset.top_p,
                            c.top_p,
                            format!("{:.2}", c.top_p),
                            0.0..=1.0,
                            0.01,
                        );
                        opt_field(
                            ui,
                            "Repeat penalty",
                            &mut preset.repeat_penalty,
                            c.repeat_penalty,
                            format!("{:.2}", c.repeat_penalty),
                            0.5..=2.0,
                            0.01,
                        );
                    });
                if preset == ModelPreset::default() {
                    c.model_presets.remove(&key);
                } else if c.model_presets.get(&key) != Some(&preset) {
                    c.model_presets.insert(key.clone(), preset);
                }
            }
            ui.add_space(4.0);
            ui.checkbox(&mut c.low_end, "Low-end machine").on_hover_text(
                "Smaller context, quantized KV cache and fewer threads for older CPUs. Applies on the next start.",
            );
        });
    }

    fn recommended_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let p = pal(ui.ctx());
        let rec = self
            .models_state
            .recommended
            .get_or_insert_with(setup::recommend_settings);
        let (threads, context, gpu, model) =
            (rec.threads, rec.context_tokens, rec.gpu_layers, rec.model);
        let path = setup::models_dir().join(model.file);
        let installed = self.models_state.recommended_installed;
        let can_act = self.models_state.download.is_none() && !self.llama_busy;
        let ram = self
            .ram_gb
            .map(|g| format!("{g} GB RAM"))
            .unwrap_or_else(|| "RAM unknown".into());
        let mut apply = false;
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "RECOMMENDED FOR THIS COMPUTER");
            let gpu_text = match gpu {
                None => "GPU auto".to_string(),
                Some(0) => "CPU only".to_string(),
                Some(n) => format!("{n} GPU layers"),
            };
            ui.label(
                RichText::new(format!(
                    "{threads} threads · {context} context · {gpu_text}"
                ))
                .strong(),
            )
            .on_hover_text(ram);
            ui.label(RichText::new(model.name).color(p.muted));
            ui.horizontal(|ui| {
                apply = ui
                    .button("Apply")
                    .on_hover_text(
                        "Set threads, context and GPU offload. Applies on the next start.",
                    )
                    .clicked();
                if installed {
                    if path != self.config.gguf_model
                        && ui
                            .add_enabled(can_act, egui::Button::new("Use model"))
                            .clicked()
                    {
                        actions.push(Action::Use(path.clone()));
                    }
                } else if ui
                    .add_enabled(can_act, egui::Button::new("Download model"))
                    .on_hover_text(format_size(model.size))
                    .clicked()
                {
                    actions.push(Action::Download {
                        file: model.file.into(),
                        url: model.url.into(),
                        size: model.size,
                        sha256: Some(model.sha256.into()),
                        purpose: Purpose::Chat { use_after: true },
                    });
                }
            });
        });
        if apply {
            self.config.llama_threads = threads;
            self.config.context_tokens = context;
            self.config.llama_gpu_layers = gpu;
            self.persist();
            self.models_state.note = "Recommended settings applied".into();
        }
    }

    fn hub_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let p = pal(ui.ctx());
        let ram = self.ram_gb;
        let s = &mut self.models_state;
        let can_download = s.download.is_none();
        let have: std::collections::HashSet<String> = s
            .installed
            .iter()
            .map(|(path, _)| file_name(path))
            .collect();
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "HUGGING FACE");
            ui.horizontal(|ui| {
                let field = ui.add(
                    egui::TextEdit::singleline(&mut s.query)
                        .hint_text("Search GGUF models")
                        .desired_width(280.0),
                );
                let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ready = s.searching.is_none() && !s.query.trim().is_empty();
                if (ui.add_enabled(ready, egui::Button::new("Search")).clicked() || enter) && ready
                {
                    actions.push(Action::Search);
                }
                if s.searching.is_some() {
                    busy(ui);
                }
            });
            for model in &s.results {
                let open = s.repo == model.repo;
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(open, short(&model.repo, 56))
                        .on_hover_text("Show GGUF files")
                        .clicked()
                    {
                        actions.push(Action::Files(model.repo.clone()));
                    }
                    ui.label(
                        RichText::new(format!("↓ {}", compact(model.downloads)))
                            .size(12.0)
                            .color(p.muted),
                    )
                    .on_hover_text(format!(
                        "{} downloads · {} likes",
                        model.downloads, model.likes
                    ));
                });
                if !open {
                    continue;
                }
                ui.indent(("hub_files", &model.repo), |ui| {
                    if s.listing.is_some() {
                        busy(ui);
                        return;
                    }
                    if s.files.is_empty() {
                        ui.label(RichText::new("No GGUF files").color(p.muted));
                        return;
                    }
                    ui.checkbox(&mut s.use_after, "Use when done");
                    egui::Grid::new(("hub_grid", &model.repo))
                        .num_columns(4)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            for f in &s.files {
                                let name = file_name(Path::new(&f.file));
                                ui.label(short(&name, 48)).on_hover_text(&f.file);
                                ui.label(RichText::new(format_size(f.size)).color(p.muted));
                                let (fit, color) = match hub::fits(f.size) {
                                    hub::FitHint::Good => ("Fits", p.ok),
                                    hub::FitHint::Tight => ("Tight", p.number),
                                    hub::FitHint::TooBig => ("Too big", Color32::LIGHT_RED),
                                };
                                ui.label(RichText::new(fit).size(12.0).color(color))
                                    .on_hover_text(match ram {
                                        Some(g) => format!("Compared with {g} GB RAM"),
                                        None => "RAM unknown".into(),
                                    });
                                if have.contains(&name) {
                                    ui.label(RichText::new("Installed").size(12.0).color(p.muted));
                                } else if ui
                                    .add_enabled(can_download, egui::Button::new("Download"))
                                    .on_hover_text(if f.sha256.is_some() {
                                        "Checked against the SHA-256 from Hugging Face"
                                    } else {
                                        "No checksum published; size is checked"
                                    })
                                    .on_disabled_hover_text("Another download is running")
                                    .clicked()
                                {
                                    actions.push(Action::Download {
                                        file: f.file.clone(),
                                        url: f.url.clone(),
                                        size: f.size,
                                        sha256: f.sha256.clone(),
                                        purpose: Purpose::Chat {
                                            use_after: s.use_after,
                                        },
                                    });
                                }
                                ui.end_row();
                            }
                        });
                });
            }
        });
    }

    fn embed_card(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let p = pal(ui.ctx());
        let have = self.models_state.embed_have;
        let running = self.embed_runtime.running();
        let starting = self.models_state.embed_start.is_some();
        let downloading = self.models_state.download.is_some();
        let c = &mut self.config;
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "EMBEDDINGS");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut c.embedding_backend, "llama".to_string(), "llama.cpp")
                    .on_hover_text("A second llama-server with --embedding");
                ui.selectable_value(&mut c.embedding_backend, "ollama".to_string(), "Ollama")
                    .on_hover_text(format!("Uses {}", c.embedding_model));
            });
            if c.embedding_backend != "llama" {
                return;
            }
            ui.horizontal(|ui| {
                if have {
                    ui.label(RichText::new(short(&file_name(&c.embedding_gguf), 48)).strong())
                        .on_hover_text(c.embedding_gguf.display().to_string());
                } else {
                    ui.label(RichText::new("No embedding model").color(p.muted));
                    let m = &setup::EMBED_MODEL;
                    if ui
                        .add_enabled(!downloading, egui::Button::new("Download nomic-embed"))
                        .on_hover_text(format!("{} · {}", m.name, format_size(m.size)))
                        .clicked()
                    {
                        actions.push(Action::Download {
                            file: m.file.into(),
                            url: m.url.into(),
                            size: m.size,
                            sha256: Some(m.sha256.into()),
                            purpose: Purpose::Embed,
                        });
                    }
                }
            });
            if !have {
                return;
            }
            ui.horizontal(|ui| {
                if running {
                    ui.label(RichText::new("Running").color(p.accent));
                    if ui.button("Stop").clicked() {
                        actions.push(Action::EmbedStop);
                    }
                } else if starting {
                    busy(ui);
                    ui.label(RichText::new("Starting…").color(p.muted));
                } else if ui
                    .button("Start")
                    .on_hover_text("Start the embedding server for retrieval")
                    .clicked()
                {
                    actions.push(Action::EmbedStart);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_counts() {
        assert_eq!(format_size(146_146_432), "146 MB");
        assert_eq!(format_size(4_683_074_336), "4.7 GB");
        assert_eq!(compact(950), "950");
        assert_eq!(compact(12_300), "12.3k");
        assert_eq!(compact(1_234_567), "1.2M");
    }

    #[test]
    fn shortens_by_characters() {
        assert_eq!(short("model.gguf", 20), "model.gguf");
        assert_eq!(short("abcdef", 4), "abc…");
        assert_eq!(short("äöüßé", 3), "äö…");
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name(Path::new("a/b/Qwen.gguf")), "Qwen.gguf");
        assert_eq!(file_name(Path::new("")), "");
    }

    #[test]
    fn take_clears_only_finished_tasks() {
        let pending: Task<u8> = Default::default();
        let mut slot = Some(pending.clone());
        assert!(take(&mut slot).is_none());
        assert!(slot.is_some());
        *pending.lock().unwrap() = Some(Ok(7));
        assert_eq!(take(&mut slot), Some(Ok(7)));
        assert!(slot.is_none());
    }
}

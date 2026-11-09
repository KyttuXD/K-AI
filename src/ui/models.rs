//! Models page: installed GGUF files with hot-swap, per-model presets,
//! recommended settings, Hugging Face downloads (with vision projectors),
//! the embedding models and server and a live performance readout. Network
//! and disk work runs on worker threads.
use super::*;
use crate::core::{hub, llama::ModelPreset};
use crate::knowledge::embeddings;
use std::{
    collections::{HashMap, HashSet},
    fs,
    ops::RangeInclusive,
    path::Path,
};

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

#[derive(Clone, PartialEq)]
enum Purpose {
    Chat {
        use_after: bool,
    },
    Embed,
    /// `ollama pull` of this embedding model.
    Ollama(String),
}

/// One file to download into the models folder, saved as `name`.
#[derive(Clone)]
struct Fetch {
    name: String,
    url: String,
    size: u64,
    sha256: Option<String>,
}

/// What a finished download produced: the model file (empty for Ollama) and,
/// when one was asked for, its vision projector or why that failed.
struct Fetched {
    model: PathBuf,
    projector: Option<Result<PathBuf, String>>,
}

struct Download {
    name: String,
    purpose: Purpose,
    progress: Arc<Mutex<setup::SetupProgress>>,
    cancel: Arc<AtomicBool>,
    task: Task<Fetched>,
}

/// What a click on the page asks for, applied after the page is drawn.
enum Action {
    Use(PathBuf),
    Delete(PathBuf),
    Download {
        fetch: Fetch,
        /// Saved next to the model and set as its preset's projector.
        projector: Option<Fetch>,
        purpose: Purpose,
    },
    OllamaPull(String),
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
    /// Also download the repository's vision projector with a model.
    with_projector: bool,
    download: Option<Download>,
    embed_start: Option<Task<u16>>,
    /// Cached by the 5 s refresh so cards never touch the disk per frame.
    active_size: Option<u64>,
    recommended_installed: bool,
    embed_have: bool,
    /// Per setup::EMBED_MODELS entry: downloaded into the models folder.
    embed_installed: Vec<bool>,
    /// Model file name → the projector llama::find_mmproj picks next to it.
    auto_mmproj: HashMap<String, PathBuf>,
    /// Model file names with a projector (chosen in the preset or found).
    vision: HashSet<String>,
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

/// A vision projector GGUF: its name contains "mmproj".
fn is_mmproj(name: &str) -> bool {
    name.to_ascii_lowercase().contains("mmproj")
}

/// A known embedding model file (setup::EMBED_MODELS).
fn is_embed_file(name: &str) -> bool {
    setup::embed_choice(name).is_some()
}

/// A quantization or precision tag such as Q4_K_M, IQ4_XS, f16 or bf16.
fn is_quant(token: &str) -> bool {
    let t = token.to_ascii_lowercase();
    if matches!(t.as_str(), "f16" | "f32" | "bf16" | "fp16" | "fp32") {
        return true;
    }
    let rest = t.strip_prefix("iq").or_else(|| t.strip_prefix('q'));
    rest.is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
}

/// A GGUF file name without ".gguf" and trailing quant tags:
/// "Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf" gives "Qwen2.5-VL-3B-Instruct".
fn model_base(name: &str) -> &str {
    let mut base = if name.to_ascii_lowercase().ends_with(".gguf") {
        &name[..name.len() - 5]
    } else {
        name
    };
    while let Some(cut) = base.rfind(['-', '.'])
        && is_quant(&base[cut + 1..])
    {
        base = &base[..cut];
    }
    base
}

/// The model name of a Hugging Face repository: "unsloth/Qwen2.5-VL-7B-Instruct-GGUF"
/// gives "Qwen2.5-VL-7B-Instruct".
fn repo_base(repo: &str) -> &str {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let lower = name.to_ascii_lowercase();
    if lower.ends_with("-gguf") || lower.ends_with("_gguf") {
        &name[..name.len() - 5]
    } else {
        name
    }
}

/// The file name a projector is saved under: kept when it already names the
/// model, else prefixed with the model's base name so "mmproj-F16.gguf" files
/// from different repositories never overwrite each other and
/// llama::find_mmproj pairs them with their model.
fn projector_name(base: &str, projector: &str) -> String {
    let name = file_name(Path::new(projector));
    if base.is_empty() || name.to_lowercase().contains(&base.to_lowercase()) {
        name
    } else {
        format!("{base}-{name}")
    }
}

/// "Delete" with an inline "Delete? Yes / No" confirmation.
fn delete_button(
    ui: &mut egui::Ui,
    confirm: &mut Option<PathBuf>,
    path: &Path,
    actions: &mut Vec<Action>,
) {
    if confirm.as_deref() == Some(path) {
        ui.label(RichText::new("Delete?").color(pal(ui.ctx()).danger_text));
        if ui.small_button("Yes").clicked() {
            actions.push(Action::Delete(path.to_path_buf()));
        }
        if ui.small_button("No").clicked() {
            *confirm = None;
        }
    } else if ui
        .small_button("Delete")
        .on_hover_text("Remove the file from disk")
        .clicked()
    {
        *confirm = Some(path.to_path_buf());
    }
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
                Ok(Fetched { model, projector }) => {
                    let mut failed = None;
                    match projector {
                        Some(Ok(mmproj)) => {
                            self.config
                                .model_presets
                                .entry(file_name(&model))
                                .or_default()
                                .mmproj = Some(mmproj);
                            self.persist();
                        }
                        Some(Err(e)) => failed = Some(e),
                        None => {}
                    }
                    match d.purpose {
                        Purpose::Embed => {
                            self.config.embedding_gguf = model;
                            self.persist();
                        }
                        Purpose::Ollama(tag) => {
                            if !self.embedding_models.contains(&tag) {
                                self.embedding_models.push(tag.clone());
                            }
                            self.config.embedding_model = tag.clone();
                            self.persist();
                            self.models_state.note = format!("Installed {tag}");
                        }
                        Purpose::Chat { use_after: true } => self.use_model(model),
                        Purpose::Chat { .. } => {
                            self.models_state.note = format!("Downloaded {}", d.name)
                        }
                    }
                    if let Some(e) = failed {
                        self.models_state.note =
                            format!("Downloaded {} without its projector: {e}", d.name);
                    }
                }
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
            // hub::installed() leaves projectors out; this page lists both (Projector rows,
            // the preset's Projector choice), sorted together by name.
            s.installed = hub::installed();
            s.installed.extend(hub::installed_projectors());
            s.installed
                .sort_by_key(|(p, _)| p.file_name().map(|n| n.to_ascii_lowercase()));
            s.installed_at = Some(Instant::now());
            // File checks happen here, not per frame: these paths may be on slow drives.
            s.active_size = fs::metadata(&self.config.gguf_model)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| m.len());
            let rec = s.recommended.get_or_insert_with(setup::recommend_settings);
            s.recommended_installed = setup::models_dir().join(rec.model.file).is_file();
            s.embed_installed = setup::EMBED_MODELS
                .iter()
                .map(|m| setup::embed_path(m).is_file())
                .collect();
            // The first downloaded known model (the default comes first) when none is set.
            if !self.config.embedding_gguf.is_file()
                && let Some(m) = setup::EMBED_MODELS
                    .iter()
                    .zip(&s.embed_installed)
                    .find_map(|(m, &have)| have.then_some(m))
            {
                self.config.embedding_gguf = setup::embed_path(m);
            }
            s.embed_have = self.config.embedding_gguf.is_file();
            s.auto_mmproj.clear();
            s.vision.clear();
            let models = s
                .installed
                .iter()
                .map(|(path, _)| path.clone())
                .chain(s.active_size.map(|_| self.config.gguf_model.clone()));
            for path in models {
                let name = file_name(&path);
                if is_mmproj(&name) || is_embed_file(&name) || s.vision.contains(&name) {
                    continue;
                }
                let chosen = self
                    .config
                    .model_presets
                    .get(&name)
                    .and_then(|p| p.mmproj.as_ref())
                    .is_some_and(|p| p.is_file());
                let auto = llama::find_mmproj(&path);
                if chosen || auto.is_some() {
                    s.vision.insert(name.clone());
                }
                if let Some(auto) = auto {
                    s.auto_mmproj.insert(name, auto);
                }
            }
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
                        // A deleted projector falls back to the automatic pick.
                        for preset in self.config.model_presets.values_mut() {
                            if preset.mmproj.as_ref() == Some(&path) {
                                preset.mmproj = None;
                            }
                        }
                        self.config
                            .model_presets
                            .retain(|_, p| *p != ModelPreset::default());
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
                fetch,
                projector,
                purpose,
            } => {
                if s.download.is_some() {
                    return;
                }
                let name = fetch.name.clone();
                let progress: Arc<Mutex<setup::SetupProgress>> = Default::default();
                let cancel = Arc::new(AtomicBool::new(false));
                let (pr, ca) = (progress.clone(), cancel.clone());
                let task = spawn_task(ctx, move || {
                    let dir = setup::models_dir();
                    fs::create_dir_all(&dir)?;
                    let get = |f: &Fetch| {
                        // file_name again: never let a name escape the models folder.
                        let dest = dir.join(file_name(Path::new(&f.name)));
                        setup::download_verified(
                            &f.url,
                            f.size,
                            f.sha256.as_deref(),
                            &dest,
                            &pr,
                            &ca,
                        )
                    };
                    let model = get(&fetch)?;
                    // A failed projector keeps the model; the note says what happened.
                    let projector = projector
                        .as_ref()
                        .map(|p| get(p).map_err(|e| format!("{e:#}")));
                    Ok(Fetched { model, projector })
                });
                s.download = Some(Download {
                    name,
                    purpose,
                    progress,
                    cancel,
                    task,
                });
            }
            Action::OllamaPull(tag) => {
                if s.download.is_some() {
                    return;
                }
                let progress: Arc<Mutex<setup::SetupProgress>> = Default::default();
                let cancel = Arc::new(AtomicBool::new(false));
                let (pr, ca, t) = (progress.clone(), cancel.clone(), tag.clone());
                let task = spawn_task(ctx, move || {
                    embeddings::ollama_pull(&t, pr, ca)?;
                    Ok(Fetched {
                        model: PathBuf::new(),
                        projector: None,
                    })
                });
                s.download = Some(Download {
                    name: tag.clone(),
                    purpose: Purpose::Ollama(tag),
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
                // The stage names the file in progress (model, then projector).
                let label = if p.stage.is_empty() {
                    format!("Downloading {}", d.name)
                } else {
                    p.stage.clone()
                };
                ui.label(short(&label, 72));
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
        let active_name = file_name(&active);
        let active_projector = self
            .config
            .model_presets
            .get(&active_name)
            .and_then(|p| p.mmproj.clone())
            .or_else(|| self.models_state.auto_mmproj.get(&active_name).cloned());
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
                        let is_embed = path == embed || is_embed_file(&name);
                        let is_projector = is_mmproj(&name);
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
                            if s.vision.contains(&name) {
                                ui.label(RichText::new("Vision").size(12.0).color(p.ok))
                                    .on_hover_text(
                                        "Has a vision projector (mmproj): can read images",
                                    );
                            }
                            if is_projector {
                                ui.label(RichText::new("Projector").size(12.0).color(p.muted))
                                    .on_hover_text(
                                        "Vision projector: loaded with its model, not on its own",
                                    );
                            } else if is_embed {
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
                            let in_use =
                                running && (is_active || active_projector.as_ref() == Some(path));
                            let deletable = path.starts_with(&models_dir) && !in_use;
                            if deletable {
                                delete_button(ui, &mut s.confirm_delete, path, actions);
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
            .filter(|n| !is_embed_file(n) && !is_mmproj(n))
            .chain((!active.is_empty()).then(|| active.clone()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let projectors: Vec<PathBuf> = self
            .models_state
            .installed
            .iter()
            .map(|(path, _)| path.clone())
            .filter(|path| is_mmproj(&file_name(path)))
            .collect();
        let s = &mut self.models_state;
        if s.preset_for.is_empty() || !names.contains(&s.preset_for) {
            s.preset_for = active.clone();
        }
        let key = s.preset_for.clone();
        let auto_text = match s.auto_mmproj.get(&key) {
            Some(path) => format!("Auto: {}", short(&file_name(path), 40)),
            None => "Auto: none".to_string(),
        };
        let mut projector_changed = false;
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
                        ui.label("Projector").on_hover_text(
                            "Vision projector (mmproj) that lets this model read images. \
                             Auto finds one next to the model. Applies on the next start.",
                        );
                        let before = preset.mmproj.clone();
                        let selected = match &preset.mmproj {
                            Some(path) => short(&file_name(path), 44),
                            None => auto_text.clone(),
                        };
                        egui::ComboBox::from_id_salt("preset_mmproj")
                            .selected_text(selected)
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut preset.mmproj, None, auto_text.as_str());
                                for path in &projectors {
                                    ui.selectable_value(
                                        &mut preset.mmproj,
                                        Some(path.clone()),
                                        short(&file_name(path), 60),
                                    )
                                    .on_hover_text(path.display().to_string());
                                }
                            });
                        projector_changed = preset.mmproj != before;
                        ui.end_row();
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
        if projector_changed {
            // Refresh the Vision tags now rather than at the next 5 s scan.
            self.models_state.installed_at = None;
        }
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
                        fetch: Fetch {
                            name: model.file.into(),
                            url: model.url.into(),
                            size: model.size,
                            sha256: Some(model.sha256.into()),
                        },
                        projector: None,
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
                    let has_projector = s.files.iter().any(|f| f.mmproj);
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut s.use_after, "Use when done");
                        if has_projector {
                            ui.checkbox(&mut s.with_projector, "With projector")
                                .on_hover_text(
                                    "Also download the vision projector (mmproj) so the \
                                     model can read images",
                                );
                        }
                    });
                    egui::Grid::new(("hub_grid", &model.repo))
                        .num_columns(4)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            for f in &s.files {
                                let shown = file_name(Path::new(&f.file));
                                // Projectors are saved under a name that pairs them with the model.
                                let name = if f.mmproj {
                                    projector_name(repo_base(&model.repo), &shown)
                                } else {
                                    shown.clone()
                                };
                                ui.label(short(&shown, 48)).on_hover_text(&f.file);
                                ui.label(RichText::new(format_size(f.size)).color(p.muted));
                                if f.mmproj {
                                    ui.label(RichText::new("Projector").size(12.0).color(p.muted))
                                        .on_hover_text(
                                            "Vision projector: lets a model of this repository read images",
                                        );
                                } else {
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
                                }
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
                                    let projector = (!f.mmproj && s.with_projector)
                                        .then(|| hub::matching_mmproj(&s.files, f))
                                        .flatten()
                                        .map(|m| Fetch {
                                            name: projector_name(
                                                model_base(&name),
                                                &file_name(Path::new(&m.file)),
                                            ),
                                            url: m.url,
                                            size: m.size,
                                            sha256: m.sha256,
                                        });
                                    actions.push(Action::Download {
                                        fetch: Fetch {
                                            name: name.clone(),
                                            url: f.url.clone(),
                                            size: f.size,
                                            sha256: f.sha256.clone(),
                                        },
                                        projector,
                                        purpose: Purpose::Chat {
                                            // A projector alone is never loaded as the model.
                                            use_after: s.use_after && !f.mmproj,
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
        let ollama_models = &self.embedding_models;
        let s = &mut self.models_state;
        let c = &mut self.config;
        let mut changed = false;
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            Self::eyebrow(ui, "EMBEDDINGS");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut c.embedding_backend, "llama".to_string(), "llama.cpp")
                    .on_hover_text("A second llama-server with --embedding");
                ui.selectable_value(&mut c.embedding_backend, "ollama".to_string(), "Ollama")
                    .on_hover_text(format!("Uses {}", c.embedding_model));
            });
            let ollama = c.embedding_backend != "llama";
            let custom = file_name(&c.embedding_gguf);
            egui::Grid::new("embed_models")
                .num_columns(3)
                .spacing([16.0, 6.0])
                .show(ui, |ui| {
                    for (i, m) in setup::EMBED_MODELS.iter().enumerate() {
                        let path = setup::embed_path(m);
                        let (installed, active) = if ollama {
                            (
                                ollama_has(ollama_models, m.ollama_tag),
                                setup::embed_choice_for_tag(&c.embedding_model)
                                    .is_some_and(|e| e.ollama_tag == m.ollama_tag),
                            )
                        } else {
                            let installed = s.embed_installed.get(i).copied().unwrap_or(false);
                            (
                                installed,
                                installed && custom.eq_ignore_ascii_case(m.model.file),
                            )
                        };
                        let label = if ollama { m.ollama_tag } else { m.model.name };
                        let text = if active {
                            RichText::new(format!("● {label}")).strong().color(p.accent)
                        } else {
                            RichText::new(label)
                        };
                        ui.label(text).on_hover_text(if ollama {
                            m.model.name
                        } else {
                            m.model.file
                        });
                        if ollama {
                            ui.label("");
                        } else {
                            ui.label(RichText::new(format_size(m.model.size)).color(p.muted));
                        }
                        ui.horizontal(|ui| {
                            if !installed {
                                let (button, hover) = if ollama {
                                    (
                                        "Install",
                                        format!(
                                            "ollama pull {} (Ollama must be running)",
                                            m.ollama_tag
                                        ),
                                    )
                                } else {
                                    (
                                        "Download",
                                        format!("{} · {}", m.model.name, format_size(m.model.size)),
                                    )
                                };
                                if ui
                                    .add_enabled(!downloading, egui::Button::new(button))
                                    .on_hover_text(hover)
                                    .on_disabled_hover_text("Another download is running")
                                    .clicked()
                                {
                                    actions.push(if ollama {
                                        Action::OllamaPull(m.ollama_tag.into())
                                    } else {
                                        Action::Download {
                                            fetch: Fetch {
                                                name: m.model.file.into(),
                                                url: m.model.url.into(),
                                                size: m.model.size,
                                                sha256: Some(m.model.sha256.into()),
                                            },
                                            projector: None,
                                            purpose: Purpose::Embed,
                                        }
                                    });
                                }
                                return;
                            }
                            if active {
                                ui.label(RichText::new("In use").size(12.0).color(p.accent));
                            } else if ui
                                .button("Use")
                                .on_hover_text("Use for Embedding rerank")
                                .clicked()
                            {
                                if ollama {
                                    c.embedding_model = m.ollama_tag.into();
                                } else {
                                    c.embedding_gguf = path.clone();
                                    if running {
                                        // The next rerank starts the server with this model.
                                        actions.push(Action::EmbedStop);
                                    }
                                }
                                changed = true;
                            }
                            // Ollama models are removed with Ollama; the served file stays.
                            if !ollama && !(active && running) {
                                delete_button(ui, &mut s.confirm_delete, &path, actions);
                            }
                        });
                        ui.end_row();
                    }
                });
            if ollama {
                return;
            }
            if have && !is_embed_file(&custom) {
                ui.label(RichText::new(format!("● {}", short(&custom, 48))).color(p.accent))
                    .on_hover_text(c.embedding_gguf.display().to_string());
            }
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
        if changed {
            self.models_state.installed_at = None;
            self.persist();
        }
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
    fn model_bases_strip_quant_tags() {
        assert_eq!(
            model_base("Qwen2.5-VL-3B-Instruct-Q4_K_M.gguf"),
            "Qwen2.5-VL-3B-Instruct"
        );
        assert_eq!(model_base("gemma-3-4b-it.F16.GGUF"), "gemma-3-4b-it");
        assert_eq!(model_base("Model-UD-IQ3_XXS.gguf"), "Model-UD");
        assert_eq!(
            model_base("llava-v1.6-mistral-7b.Q5_K_M.gguf"),
            "llava-v1.6-mistral-7b"
        );
        assert_eq!(model_base("plain.gguf"), "plain");
        assert_eq!(model_base("Qwen3-4B-q8_0-bf16.gguf"), "Qwen3-4B");
        assert!(is_quant("Q4_K_M") && is_quant("iq2_xs") && is_quant("bf16"));
        assert!(!is_quant("Instruct") && !is_quant("q") && !is_quant("qwen"));
    }

    #[test]
    fn projector_names_pair_with_their_model() {
        assert_eq!(
            repo_base("unsloth/Qwen2.5-VL-7B-Instruct-GGUF"),
            "Qwen2.5-VL-7B-Instruct"
        );
        assert_eq!(repo_base("ggml-org/gemma-3-4b-it_gguf"), "gemma-3-4b-it");
        assert_eq!(repo_base("owner/plain"), "plain");
        // Generic names get the model's base so repositories never collide.
        assert_eq!(
            projector_name("Qwen2.5-VL-7B-Instruct", "mmproj-F16.gguf"),
            "Qwen2.5-VL-7B-Instruct-mmproj-F16.gguf"
        );
        // Names that already name the model (any case) are kept.
        assert_eq!(
            projector_name("gemma-3-4b-it", "sub/mmproj-Gemma-3-4B-it-f16.gguf"),
            "mmproj-Gemma-3-4B-it-f16.gguf"
        );
        assert_eq!(projector_name("", "mmproj.gguf"), "mmproj.gguf");
        assert!(is_mmproj("Model-MMPROJ-f16.gguf") && !is_mmproj("Model-Q4_K_M.gguf"));
    }

    #[test]
    fn known_embedding_files() {
        assert!(is_embed_file(setup::EMBED_MODEL.file));
        assert!(
            setup::EMBED_MODELS
                .iter()
                .all(|m| is_embed_file(m.model.file))
        );
        assert!(!is_embed_file("Qwen2.5-3B-Instruct-Q4_K_M.gguf"));
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

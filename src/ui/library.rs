use super::*;
use crate::knowledge::{
    docs::{self, DocSource, Progress, SourceMode},
    watch::DocWatcher,
};
use std::{collections::HashMap, path::Path};

/// Extensions offered by "Add files…"; keep in step with `docs::supported`.
const EXTENSIONS: &[&str] = &[
    "md", "mdx", "markdown", "txt", "rst", "adoc", "asciidoc", "html", "htm", "pdf", "docx",
];
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv"];

/// Paths waiting for the Link / Import choice.
struct Pending {
    paths: Vec<PathBuf>,
    import: bool,
    ignored: usize,
}

type ImportJob = (Arc<Mutex<Progress>>, Receiver<anyhow::Result<DocSource>>);

#[derive(Default)]
pub struct LibraryState {
    watcher: Option<DocWatcher>,
    /// Sources the watcher, packs and counts were set up for; `None` before the first frame.
    synced: Option<Vec<DocSource>>,
    rebuild_due: bool,
    was_running: bool,
    /// Supported file count per source id, filled by a background walk.
    counts: Arc<Mutex<HashMap<String, usize>>>,
    pending: Option<Pending>,
    import: Option<ImportJob>,
    query: String,
    pack: String,
    results: Vec<Source>,
    searched: bool,
}

enum RowAction {
    Toggle(usize),
    Rebuild(String),
    Remove(String),
}

impl Studio {
    pub(super) fn library_page(&mut self, ui: &mut egui::Ui) {
        let pl = pal(ui.ctx());
        Self::page_title(ui, "Library");
        let p = self.library.progress.lock().unwrap().clone();
        let importing = self.library_state.import.is_some();
        egui::ScrollArea::vertical()
            .id_salt("library_page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.config.doc_sources.is_empty() && !importing {
                    self.library_empty(ui);
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{} files · {} passages", p.files, p.chunks))
                            .strong(),
                    )
                    .on_hover_text(
                        "Markdown, text, RST, AsciiDoc, HTML, PDF and DOCX are indexed.",
                    );
                    if p.running {
                        busy(ui);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let free = !importing;
                        if ui
                            .add_enabled(free, egui::Button::new("Add files…"))
                            .clicked()
                        {
                            self.pick_doc_files();
                        }
                        if ui
                            .add_enabled(free, egui::Button::new("Add folder…"))
                            .on_hover_text("Or drop files and folders on the window")
                            .clicked()
                        {
                            self.pick_doc_folder();
                        }
                    });
                });
                ui.add_space(8.0);
                self.source_rows(ui, p.running);
                if let Some((progress, _)) = &self.library_state.import {
                    let ip = progress.lock().unwrap().clone();
                    Self::card(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            busy(ui);
                            ui.label(
                                RichText::new(format!("Importing · {} files", ip.files)).strong(),
                            );
                        });
                        if !ip.message.is_empty() {
                            ui.label(RichText::new(&ip.message).size(11.5).color(pl.muted));
                        }
                        skipped_list(ui, &ip, "import_skipped");
                    });
                    ui.add_space(8.0);
                }
                let mut note = p.message.clone();
                if p.skipped > 0 {
                    note.push_str(&format!(" · {} skipped", p.skipped));
                }
                if !note.trim().is_empty() {
                    ui.label(RichText::new(note).size(11.5).color(pl.muted));
                }
                skipped_list(ui, &p, "skipped_files");
                ui.add_space(12.0);
                self.library_search(ui);
            });
    }

    fn library_empty(&mut self, ui: &mut egui::Ui) {
        let pl = pal(ui.ctx());
        Self::card(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("Add documentation").size(16.0).strong());
                ui.label(
                    RichText::new("Or drop files and folders anywhere in this window.")
                        .size(12.0)
                        .color(pl.muted),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    // Centre the two buttons inside the vertical_centered column.
                    ui.add_space(((ui.available_width() - 220.0) / 2.0).max(0.0));
                    if Self::primary(ui, "Add folder…")
                        .on_hover_text("Linked in place by default")
                        .clicked()
                    {
                        self.pick_doc_folder();
                    }
                    if ui
                        .button("Add files…")
                        .on_hover_text("Imported as a copy by default")
                        .clicked()
                    {
                        self.pick_doc_files();
                    }
                });
                ui.add_space(12.0);
            });
        });
    }

    fn source_rows(&mut self, ui: &mut egui::Ui, running: bool) {
        let pl = pal(ui.ctx());
        let counts = self.library_state.counts.lock().unwrap().clone();
        let mut action = None;
        for (i, s) in self.config.doc_sources.iter().enumerate() {
            Self::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let mut enabled = s.enabled;
                    if ui
                        .checkbox(&mut enabled, "")
                        .on_hover_text("Include in search and chat")
                        .changed()
                    {
                        action = Some(RowAction::Toggle(i));
                    }
                    let name = RichText::new(&s.name).strong();
                    ui.label(if s.enabled {
                        name
                    } else {
                        name.color(pl.muted)
                    });
                    let (mode, hint) = match s.mode {
                        SourceMode::Linked => ("Linked", "Indexed in place; changes are picked up"),
                        SourceMode::Imported => ("Imported", "A copy in the app's library folder"),
                    };
                    ui.label(RichText::new(mode).size(11.5).color(pl.muted))
                        .on_hover_text(hint);
                    if let Some(n) = counts.get(&s.id) {
                        ui.label(
                            RichText::new(format!(
                                "{n} {}",
                                if *n == 1 { "file" } else { "files" }
                            ))
                            .size(11.5)
                            .color(pl.muted),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(!running, |ui| {
                            ui.menu_button(icons::MORE, |ui| {
                                if ui
                                    .add_enabled(
                                        s.enabled,
                                        egui::Button::new(icons::label(icons::REFRESH, "Rebuild")),
                                    )
                                    .on_hover_text("Re-read every file of this source")
                                    .clicked()
                                {
                                    action = Some(RowAction::Rebuild(s.id.clone()));
                                    ui.close();
                                }
                                if ui
                                    .button(icons::label(icons::TRASH, "Remove"))
                                    .on_hover_text(match s.mode {
                                        SourceMode::Imported => {
                                            "Removes the imported copy (Undo available)"
                                        }
                                        SourceMode::Linked => "The original files are kept",
                                    })
                                    .clicked()
                                {
                                    action = Some(RowAction::Remove(s.id.clone()));
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text("More");
                        });
                    });
                });
                let path = s.path.display().to_string();
                ui.add(
                    egui::Label::new(RichText::new(&path).size(11.5).monospace().color(pl.muted))
                        .truncate(),
                )
                .on_hover_text(path);
            });
            ui.add_space(6.0);
        }
        match action {
            Some(RowAction::Toggle(i)) => {
                let s = &mut self.config.doc_sources[i];
                s.enabled = !s.enabled;
                self.persist();
            }
            Some(RowAction::Rebuild(id)) => {
                self.library.remove_source(&id);
                self.library_state.rebuild_due = true;
            }
            Some(RowAction::Remove(id)) => {
                if let Some(source) = self.config.doc_sources.iter().find(|s| s.id == id).cloned() {
                    self.remove_doc_source(&source);
                }
            }
            None => {}
        }
    }

    fn library_search(&mut self, ui: &mut egui::Ui) {
        let pl = pal(ui.ctx());
        let st = &mut self.library_state;
        let mut run = false;
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut st.query)
                    .hint_text("Search documentation")
                    .desired_width((ui.available_width() - 250.0).max(120.0)),
            );
            run = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            egui::ComboBox::from_id_salt("library_pack")
                .selected_text(if st.pack.is_empty() { "All" } else { &st.pack })
                .width(150.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut st.pack, String::new(), "All");
                    for pack in &self.packs {
                        ui.selectable_value(&mut st.pack, pack.clone(), pack);
                    }
                });
            run |= ui.button(icons::label(icons::SEARCH, "Search")).clicked();
        });
        if run {
            st.searched = true;
            match self.library.search(&st.query, &st.pack, 8) {
                Ok(found) => st.results = found,
                Err(e) => self.error = Some(format!("Search failed: {e:#}")),
            }
        }
        ui.add_space(8.0);
        if st.searched && st.results.is_empty() && !st.query.trim().is_empty() {
            ui.label(RichText::new("No matches").size(12.0).color(pl.muted));
        }
        for (i, s) in st.results.iter().enumerate() {
            let (crumb, body) = split_breadcrumb(&s.text);
            Self::card(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("[{}] {}:{}", i + 1, s.path, s.line))
                            .strong()
                            .color(pl.accent),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(s.text.clone());
                        }
                    });
                });
                if let Some(crumb) = crumb {
                    ui.label(RichText::new(crumb).size(11.5).color(pl.muted));
                }
                ui.label(body);
            });
            ui.add_space(8.0);
        }
    }

    /// Keeps the watcher, packs, counts and index in step with `config.doc_sources`, finishes
    /// imports, and handles drops and the Library dialogs. Call once per frame.
    pub(super) fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        self.sync_library(ctx);
        let (hovering, dropped) = ctx.input(|i| {
            (
                // Images and audio dropped on Chat are attachments, not documentation.
                i.raw.hovered_files.iter().any(|f| {
                    self.view != 0
                        || f.path
                            .as_deref()
                            .is_none_or(|p| image_mime(p).is_none() && audio_mime(p).is_none())
                }),
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect::<Vec<_>>(),
            )
        });
        if hovering {
            let rect = ctx.content_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("library_drop"),
            ));
            painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Drop to add to Library",
                FontId::proportional(20.0),
                Color32::WHITE,
            );
        }
        if !dropped.is_empty() {
            self.offer_docs(dropped);
        }
        self.library_dialogs(ctx);
    }

    fn sync_library(&mut self, ctx: &egui::Context) {
        let received = self
            .library_state
            .import
            .as_ref()
            .and_then(|(_, rx)| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("stopped"))),
            });
        if let Some(result) = received {
            self.library_state.import = None;
            match result {
                Ok(mut source) => {
                    source.name = unique_name(&source.name, &names(&self.config.doc_sources));
                    self.config.doc_sources.push(source);
                    self.persist();
                }
                Err(e) => self.error = Some(format!("Import failed: {e:#}")),
            }
        }
        let running = self.library.progress.lock().unwrap().running;
        let st = &mut self.library_state;
        let sources = &self.config.doc_sources;
        let mut recount = st.was_running && !running;
        st.was_running = running;
        if st.synced.as_ref() != Some(sources) {
            if st.synced.is_some() || !self.library.matches_sources(sources) {
                st.rebuild_due = true;
            }
            st.watcher = None;
            let enabled: Vec<DocSource> = sources.iter().filter(|s| s.enabled).cloned().collect();
            if !enabled.is_empty() {
                match DocWatcher::start(&enabled, self.library.clone()) {
                    Ok(w) => st.watcher = Some(w),
                    Err(e) => self.error = Some(format!("Not watching documentation: {e:#}")),
                }
            }
            self.packs = enabled.iter().map(|s| s.name.clone()).collect();
            self.packs.sort();
            if !self.packs.contains(&self.pack) {
                self.pack.clear();
            }
            if !self.packs.contains(&st.pack) {
                st.pack.clear();
            }
            st.synced = Some(sources.clone());
            recount = true;
        }
        if st.rebuild_due && !running {
            st.rebuild_due = false;
            self.library.rebuild_sources(sources.clone());
        }
        if recount {
            let (list, counts) = (sources.clone(), st.counts.clone());
            std::thread::spawn(move || {
                let fresh: HashMap<String, usize> = list
                    .iter()
                    .map(|s| (s.id.clone(), count_files(&s.path)))
                    .collect();
                *counts.lock().unwrap() = fresh;
            });
        }
        if running || st.import.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn pick_doc_folder(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.offer_docs(vec![path]);
        }
    }
    fn pick_doc_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("Documents", EXTENSIONS)
            .pick_files()
        {
            self.offer_docs(paths);
        }
    }

    /// Asks Link vs Import for the supported paths: Import for files only, Link when a
    /// folder is among them.
    fn offer_docs(&mut self, paths: Vec<PathBuf>) {
        if self.library_state.import.is_some() || self.library_state.pending.is_some() {
            self.error = Some("Wait for the current import to finish.".into());
            return;
        }
        let total = paths.len();
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| p.is_dir() || docs::supported(p))
            .collect();
        if paths.is_empty() {
            self.error = Some(
                "No supported documents. Use Markdown, text, RST, AsciiDoc, HTML, PDF or DOCX."
                    .into(),
            );
            return;
        }
        self.library_state.pending = Some(Pending {
            import: !paths.iter().any(|p| p.is_dir()),
            ignored: total - paths.len(),
            paths,
        });
    }

    fn library_dialogs(&mut self, ctx: &egui::Context) {
        let pl = pal(ctx);
        if let Some(pending) = &mut self.library_state.pending {
            let mut add = None;
            let modal = egui::Modal::new(egui::Id::new("library_add")).show(ctx, |ui| {
                ui.set_width(360.0);
                Self::eyebrow(ui, "ADD DOCUMENTATION");
                ui.label(RichText::new(describe(&pending.paths)).strong());
                ui.add_space(6.0);
                ui.radio_value(&mut pending.import, false, "Link in place")
                    .on_hover_text("Index the originals where they are; edits are picked up");
                ui.radio_value(&mut pending.import, true, "Import a copy")
                    .on_hover_text("Copy into the app's library folder; the originals can move");
                if pending.ignored > 0 {
                    ui.label(
                        RichText::new(format!("{} unsupported ignored", pending.ignored))
                            .size(11.5)
                            .color(pl.muted),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if Self::primary(ui, "Add").clicked() {
                        add = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        add = Some(false);
                    }
                });
            });
            if add.is_none() && modal.should_close() {
                add = Some(false);
            }
            if let Some(add) = add
                && let Some(pending) = self.library_state.pending.take()
                && add
            {
                self.add_doc_sources(pending);
            }
        }
    }

    fn add_doc_sources(&mut self, pending: Pending) {
        if pending.import {
            let name = unique_name(
                &source_name(&pending.paths),
                &names(&self.config.doc_sources),
            );
            let progress = Arc::new(Mutex::new(Progress {
                running: true,
                message: "Copying…".into(),
                ..Default::default()
            }));
            let (tx, rx) = mpsc::channel();
            let (paths, shared) = (pending.paths, progress.clone());
            std::thread::spawn(move || {
                let _ = tx.send(docs::import_paths(&paths, &name, shared));
            });
            self.library_state.import = Some((progress, rx));
            return;
        }
        for path in pending.paths {
            let linked = |s: &DocSource| s.mode == SourceMode::Linked && s.path == path;
            if self.config.doc_sources.iter().any(linked) {
                continue;
            }
            let mut source = docs::link_path(&path);
            source.name = unique_name(&source.name, &names(&self.config.doc_sources));
            self.config.doc_sources.push(source);
        }
        self.persist();
    }

    /// Removes a source at once. Imported copies are moved to the trash so Undo can bring
    /// them back; they are really deleted when the toast expires.
    fn remove_doc_source(&mut self, source: &DocSource) {
        let staged = if source.mode == SourceMode::Imported && source.path.exists() {
            match crate::core::trash::stage(&source.path) {
                Ok(staged) => Some(staged),
                Err(e) => {
                    self.error_toast(format!("Could not remove the imported copy: {e:#}"));
                    return;
                }
            }
        } else {
            None
        };
        self.library.remove_source(&source.id);
        let index = self
            .config
            .doc_sources
            .iter()
            .position(|s| s.id == source.id)
            .unwrap_or(self.config.doc_sources.len());
        self.config.doc_sources.retain(|s| s.id != source.id);
        self.library_state.results.clear();
        self.persist();
        let kept = source.clone();
        let held = Arc::new(Mutex::new(staged));
        let held_undo = held.clone();
        self.undo_toast(
            format!("Removed {}", source.name),
            Box::new(move |studio: &mut Studio| {
                let staged = held_undo.lock().unwrap().take();
                if let Some(staged) = staged
                    && let Err(e) = staged.restore()
                {
                    studio.error_toast(format!("Could not restore the imported copy: {e:#}"));
                    return;
                }
                let at = index.min(studio.config.doc_sources.len());
                studio.config.doc_sources.insert(at, kept);
                studio.library_state.rebuild_due = true;
                studio.persist();
            }),
            Box::new(move || {
                let staged = held.lock().unwrap().take();
                if let Some(staged) = staged {
                    let _ = staged.commit();
                }
            }),
        );
    }
}

fn skipped_list(ui: &mut egui::Ui, p: &Progress, salt: &str) {
    if p.skipped_files.is_empty() {
        return;
    }
    let muted = pal(ui.ctx()).muted;
    egui::CollapsingHeader::new(format!("Skipped files ({})", p.skipped))
        .id_salt(salt)
        .show(ui, |ui| {
            for (path, reason) in &p.skipped_files {
                ui.label(
                    RichText::new(format!("{path}: {reason}"))
                        .size(11.5)
                        .color(muted),
                );
            }
            if p.skipped > p.skipped_files.len() {
                ui.label(
                    RichText::new(format!("…and {} more", p.skipped - p.skipped_files.len()))
                        .size(11.5)
                        .color(muted),
                );
            }
        });
}

fn names(sources: &[DocSource]) -> Vec<String> {
    sources.iter().map(|s| s.name.clone()).collect()
}

/// `base`, or `base 2`, `base 3`… when a source already uses that name (case-insensitive).
fn unique_name(base: &str, taken: &[String]) -> String {
    let base = if base.trim().is_empty() {
        "Documents"
    } else {
        base.trim()
    };
    let used = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if !used(base) {
        return base.to_owned();
    }
    (2..)
        .map(|i| format!("{base} {i}"))
        .find(|n| !used(n))
        .unwrap()
}

/// A name for an import: the folder or file stem for one path, else the first path's parent folder.
fn source_name(paths: &[PathBuf]) -> String {
    let stem = |p: &Path| {
        let part = if p.is_dir() {
            p.file_name()
        } else {
            p.file_stem()
        };
        part.map(|s| s.to_string_lossy().into_owned())
    };
    match paths {
        [one] => stem(one.as_path()),
        [first, ..] => first
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned()),
        [] => None,
    }
    .unwrap_or_else(|| "Documents".into())
}

fn describe(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one.file_name().map_or_else(
            || one.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
        _ => format!("{} items", paths.len()),
    }
}

/// Splits the "[File > H1 > H2]\n" prefix that `Library::search` puts on passages.
fn split_breadcrumb(text: &str) -> (Option<&str>, &str) {
    if let Some(rest) = text.strip_prefix('[')
        && let Some((crumb, body)) = rest.split_once("]\n")
        && !crumb.contains('\n')
    {
        return (Some(crumb), body);
    }
    (None, text)
}

fn count_files(path: &Path) -> usize {
    if path.is_file() {
        return usize::from(docs::supported(path));
    }
    walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .is_some_and(|n| SKIP_DIRS.contains(&n))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && docs::supported(e.path()))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_get_a_number() {
        let taken = vec!["Rust".to_owned(), "rust 2".to_owned()];
        assert_eq!(unique_name("Godot", &taken), "Godot");
        assert_eq!(unique_name("rust", &taken), "rust 3");
        assert_eq!(unique_name("  ", &[]), "Documents");
    }

    #[test]
    fn breadcrumb_is_split_off() {
        assert_eq!(
            split_breadcrumb("[guide.md > Setup]\nRun it."),
            (Some("guide.md > Setup"), "Run it.")
        );
        assert_eq!(split_breadcrumb("[x] plain"), (None, "[x] plain"));
        assert_eq!(split_breadcrumb("[a\nb]\nc"), (None, "[a\nb]\nc"));
    }

    #[test]
    fn names_and_counts_from_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let docs_dir = dir.path().join("Manual");
        std::fs::create_dir_all(docs_dir.join("sub"))?;
        std::fs::create_dir_all(docs_dir.join(".git"))?;
        std::fs::write(docs_dir.join("a.md"), "# A")?;
        std::fs::write(docs_dir.join("sub/b.txt"), "b")?;
        std::fs::write(docs_dir.join("c.exe"), "")?;
        std::fs::write(docs_dir.join(".git/d.md"), "")?;
        assert_eq!(count_files(&docs_dir), 2);
        assert_eq!(count_files(&docs_dir.join("a.md")), 1);
        assert_eq!(source_name(std::slice::from_ref(&docs_dir)), "Manual");
        assert_eq!(source_name(&[docs_dir.join("a.md")]), "a");
        let two = [docs_dir.join("a.md"), docs_dir.join("sub/b.txt")];
        assert_eq!(source_name(&two), "Manual");
        assert_eq!(describe(&two), "2 items");
        assert_eq!(describe(&[docs_dir.join("a.md")]), "a.md");
        Ok(())
    }
}

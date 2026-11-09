//! One stack of notices in the bottom-right corner: short info toasts that fade by themselves, errors that stay until
//! dismissed, and undo toasts for deletions. An undo toast carries two closures: `undo` brings the item back, and
//! `expire` does the real delete. Exactly one of them runs, when the toast is undone, times out, is dismissed, or
//! the app exits (`flush_notices`).
use super::*;
use crate::ui::icons;

/// Longest the stack shows at once; older notices wait underneath and keep their timers.
const MAX_VISIBLE: usize = 4;
const INFO_SECS: f32 = 4.0;
const UNDO_SECS: f32 = 8.0;
const FADE_IN: f32 = 0.15;
const FADE_OUT: f32 = 0.5;
const MAX_CHARS: usize = 600;
const WIDTH: f32 = 320.0;

type UndoFn = Box<dyn FnOnce(&mut Studio) + Send>;
type ExpireFn = Box<dyn FnOnce() + Send>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Info,
    Error,
    Undo,
}

struct Notice {
    id: u64,
    kind: Kind,
    text: String,
    born: Instant,
    undo: Option<UndoFn>,
    expire: Option<ExpireFn>,
}

/// The notice stack. Lives in `Studio` as `notices`.
#[derive(Default)]
pub struct Notices {
    items: Vec<Notice>,
    next_id: u64,
}

/// How long a kind stays; `None` stays until dismissed.
fn lifetime(kind: Kind) -> Option<Duration> {
    match kind {
        Kind::Info => Some(Duration::from_secs_f32(INFO_SECS)),
        Kind::Undo => Some(Duration::from_secs_f32(UNDO_SECS)),
        Kind::Error => None,
    }
}

fn is_expired(kind: Kind, age: Duration) -> bool {
    lifetime(kind).is_some_and(|l| age >= l)
}

/// Opacity from 0 to 1: a short fade in, a longer fade out at the end of the lifetime.
fn fade_alpha(kind: Kind, age: Duration) -> f32 {
    let age = age.as_secs_f32();
    let fade_in = (age / FADE_IN).clamp(0.0, 1.0);
    let fade_out = lifetime(kind).map_or(1.0, |l| {
        ((l.as_secs_f32() - age) / FADE_OUT).clamp(0.0, 1.0)
    });
    fade_in.min(fade_out)
}

/// Index of the first notice that is drawn (the newest `MAX_VISIBLE` are).
fn visible_start(len: usize) -> usize {
    len.saturating_sub(MAX_VISIBLE)
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    let mut short: String = text.chars().take(MAX_CHARS).collect();
    short.push('…');
    short
}

impl Notices {
    /// Adds a notice. A repeat of a live error or info text only restarts that notice instead of stacking.
    fn push(&mut self, kind: Kind, text: String, now: Instant) -> Option<&mut Notice> {
        let text = clip(text);
        if kind != Kind::Undo
            && let Some(i) = self
                .items
                .iter()
                .position(|n| n.kind == kind && n.text == text)
        {
            self.items[i].born = now;
            return None;
        }
        self.next_id += 1;
        self.items.push(Notice {
            id: self.next_id,
            kind,
            text,
            born: now,
            undo: None,
            expire: None,
        });
        self.items.last_mut()
    }

    /// Removes timed-out notices and returns their expire closures to run.
    fn take_expired(&mut self, now: Instant) -> Vec<ExpireFn> {
        let mut due = Vec::new();
        self.items.retain_mut(|n| {
            if is_expired(n.kind, now.saturating_duration_since(n.born)) {
                due.extend(n.expire.take());
                false
            } else {
                true
            }
        });
        due
    }

    /// Removes one notice; its expire closure comes back (a dismissal counts as expiry).
    fn dismiss(&mut self, id: u64) -> Option<ExpireFn> {
        let i = self.items.iter().position(|n| n.id == id)?;
        self.items.remove(i).expire
    }

    /// Removes one notice for Undo: the undo closure comes back and the expire closure is dropped unrun.
    fn take_undo(&mut self, id: u64) -> Option<UndoFn> {
        let i = self.items.iter().position(|n| n.id == id)?;
        self.items.remove(i).undo
    }

    /// Removes everything and returns every pending expire closure.
    fn drain_expire(&mut self) -> Vec<ExpireFn> {
        self.items
            .drain(..)
            .filter_map(|mut n| n.expire.take())
            .collect()
    }
}

enum Action {
    Undo(u64),
    Dismiss(u64),
}

impl Studio {
    /// Info toast: fades after a few seconds.
    pub(super) fn toast(&mut self, text: impl Into<String>) {
        self.notices.push(Kind::Info, text.into(), Instant::now());
    }

    /// Error toast: stays until dismissed; the same text is shown once.
    pub(super) fn error_toast(&mut self, text: impl Into<String>) {
        self.notices.push(Kind::Error, text.into(), Instant::now());
    }

    /// Undo toast for a deletion done at once. `undo` brings the item back; otherwise `expire` runs when the toast
    /// times out, is dismissed or the app exits, and does the real delete.
    pub(super) fn undo_toast(
        &mut self,
        text: impl Into<String>,
        undo: Box<dyn FnOnce(&mut Studio) + Send>,
        expire: Box<dyn FnOnce() + Send>,
    ) {
        if let Some(n) = self.notices.push(Kind::Undo, text.into(), Instant::now()) {
            n.undo = Some(undo);
            n.expire = Some(expire);
        }
    }

    /// Runs pending expire closures (the real deletes). Called from `on_exit`.
    pub(super) fn flush_notices(&mut self) {
        for expire in self.notices.drain_expire() {
            expire();
        }
    }

    /// Draws the stack above the composer in the bottom-right corner. It floats over the page and never moves it.
    pub(super) fn show_notices(&mut self, ctx: &egui::Context) {
        for expire in self.notices.take_expired(Instant::now()) {
            expire();
        }
        if self.notices.items.is_empty() {
            return;
        }
        let p = pal(ctx);
        let now = Instant::now();
        let mut action = None;
        let mut animating = false;
        let first = visible_start(self.notices.items.len());
        egui::Area::new(egui::Id::new("kyttu_notices"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -96.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                for n in &self.notices.items[first..] {
                    let age = now.saturating_duration_since(n.born);
                    let alpha = fade_alpha(n.kind, age);
                    animating |= alpha < 1.0;
                    ui.scope(|ui| {
                        ui.multiply_opacity(alpha);
                        if let Some(a) = draw_notice(ui, &p, n) {
                            action = Some(a);
                        }
                    });
                }
            });
        // Fades need smooth frames; between them a slow tick is enough for the timers.
        if animating {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        match action {
            Some(Action::Undo(id)) => {
                if let Some(undo) = self.notices.take_undo(id) {
                    undo(self);
                }
            }
            Some(Action::Dismiss(id)) => {
                if let Some(expire) = self.notices.dismiss(id) {
                    expire();
                }
            }
            None => {}
        }
    }
}

fn draw_notice(ui: &mut egui::Ui, p: &Palette, n: &Notice) -> Option<Action> {
    let (icon, color) = match n.kind {
        Kind::Info => (icons::CHECK, p.accent),
        Kind::Error => (icons::ERROR, Color32::from_rgb(235, 87, 87)),
        Kind::Undo => (icons::TRASH, p.muted),
    };
    let mut action = None;
    let frame = egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(WIDTH);
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).color(color));
                ui.add(egui::Label::new(RichText::new(&n.text).color(p.text)).wrap());
            });
            if n.kind != Kind::Info {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(icons::CLOSE)
                        .on_hover_text("Dismiss")
                        .clicked()
                    {
                        action = Some(Action::Dismiss(n.id));
                    }
                    if n.kind == Kind::Undo
                        && ui
                            .button(
                                RichText::new(icons::label(icons::UNDO, "Undo")).color(p.accent),
                            )
                            .clicked()
                    {
                        action = Some(Action::Undo(n.id));
                    }
                });
            }
        });
    if n.kind == Kind::Info
        && frame
            .response
            .interact(egui::Sense::click())
            .on_hover_text("Click to dismiss")
            .clicked()
    {
        action = Some(Action::Dismiss(n.id));
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    #[test]
    fn lifetimes() {
        assert!(!is_expired(Kind::Info, secs(3.9)));
        assert!(is_expired(Kind::Info, secs(4.0)));
        assert!(!is_expired(Kind::Undo, secs(7.9)));
        assert!(is_expired(Kind::Undo, secs(8.0)));
        assert!(!is_expired(Kind::Error, secs(10_000.0)));
    }

    #[test]
    fn fade_curve() {
        assert_eq!(fade_alpha(Kind::Info, Duration::ZERO), 0.0);
        assert_eq!(fade_alpha(Kind::Info, secs(1.0)), 1.0);
        let end = fade_alpha(Kind::Info, secs(3.8));
        assert!(end > 0.0 && end < 1.0);
        assert_eq!(fade_alpha(Kind::Info, secs(4.0)), 0.0);
        assert_eq!(fade_alpha(Kind::Error, secs(500.0)), 1.0);
    }

    #[test]
    fn newest_are_visible() {
        assert_eq!(visible_start(0), 0);
        assert_eq!(visible_start(4), 0);
        assert_eq!(visible_start(5), 1);
        assert_eq!(visible_start(10), 6);
    }

    #[test]
    fn errors_are_deduplicated() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        assert!(n.push(Kind::Error, "boom".into(), t0).is_some());
        assert!(n.push(Kind::Error, "boom".into(), t0).is_none());
        assert!(n.push(Kind::Error, "other".into(), t0).is_some());
        assert!(n.push(Kind::Info, "boom".into(), t0).is_some());
        assert_eq!(n.items.len(), 3);
    }

    #[test]
    fn repeated_info_restarts_its_timer() {
        let mut n = Notices::default();
        let t0 = Instant::now();
        n.push(Kind::Info, "hi".into(), t0);
        n.push(Kind::Info, "hi".into(), t0 + secs(3.0));
        assert_eq!(n.items.len(), 1);
        assert!(n.take_expired(t0 + secs(5.0)).is_empty());
        assert!(n.items.len() == 1);
        n.take_expired(t0 + secs(8.0));
        assert!(n.items.is_empty());
    }

    #[test]
    fn long_text_is_clipped() {
        let long = "x".repeat(MAX_CHARS + 50);
        assert_eq!(clip(long).chars().count(), MAX_CHARS + 1);
        assert_eq!(clip("short".into()), "short");
    }

    fn counting_undo(n: &mut Notices, t0: Instant, ran: &Arc<AtomicUsize>) -> u64 {
        let r = ran.clone();
        let item = n
            .push(Kind::Undo, format!("deleted {}", n.next_id), t0)
            .unwrap();
        item.expire = Some(Box::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        item.undo = Some(Box::new(|_| {}));
        item.id
    }

    #[test]
    fn expire_runs_once_on_timeout() {
        let mut n = Notices::default();
        let ran = Arc::new(AtomicUsize::new(0));
        let t0 = Instant::now();
        counting_undo(&mut n, t0, &ran);
        assert!(n.take_expired(t0 + secs(7.0)).is_empty());
        for f in n.take_expired(t0 + secs(8.5)) {
            f();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(n.take_expired(t0 + secs(60.0)).is_empty());
    }

    #[test]
    fn undo_drops_expire() {
        let mut n = Notices::default();
        let ran = Arc::new(AtomicUsize::new(0));
        let id = counting_undo(&mut n, Instant::now(), &ran);
        assert!(n.take_undo(id).is_some());
        assert!(n.drain_expire().is_empty());
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dismiss_and_exit_run_expire() {
        let mut n = Notices::default();
        let ran = Arc::new(AtomicUsize::new(0));
        let t0 = Instant::now();
        let a = counting_undo(&mut n, t0, &ran);
        counting_undo(&mut n, t0, &ran);
        n.dismiss(a).unwrap()();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(n.dismiss(a).is_none());
        for f in n.drain_expire() {
            f();
        }
        assert_eq!(ran.load(Ordering::SeqCst), 2);
        assert!(n.items.is_empty());
    }
}

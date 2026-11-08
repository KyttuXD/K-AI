#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]
mod core;
mod integrations;
mod knowledge;
mod ui;
use core::{config, engine, tools};
use knowledge::docs;

fn main() -> eframe::Result {
    core::diagnostics::install_panic_hook();
    core::update::cleanup_old();
    let (config, notice) = config::Config::load_checked();
    // Logical points; the OS display scale multiplies these, so keep the
    // default small enough to fit a 1366x768 screen at 125%. A remembered
    // size that no longer fits is shrunk by Studio::fit_to_monitor.
    let size = config
        .window_size
        .filter(|s| s.iter().all(|v| v.is_finite() && *v >= 320.0))
        .unwrap_or([1100.0, 720.0]);
    // The position is not restored: egui cannot tell whether a saved spot is still on a
    // connected monitor, and a window placed off-screen cannot be reached.
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title("KYTTU - AI")
        .with_inner_size(size)
        .with_min_inner_size([640.0, 440.0]);
    eframe::run_native(
        "KYTTU - AI",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(ui::Studio::new(cc, config, notice)))),
    )
}

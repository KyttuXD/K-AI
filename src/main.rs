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
    // Files held for a last Undo in an earlier run are deleted for good.
    core::trash::purge_old();
    let (config, notice) = config::Config::load_checked();
    // Logical points; the OS display scale multiplies these, so keep the
    // default small enough to fit a 1366x768 screen at 125%. Once the monitor
    // is known, Studio::fit_to_monitor restores the geometry saved for its size
    // (config.monitor_windows) or shrinks a size that no longer fits.
    let size = config
        .window_size
        .filter(|s| s.iter().all(|v| v.is_finite() && *v >= 320.0))
        .unwrap_or([1100.0, 720.0]);
    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title("KYTTU - AI")
        .with_inner_size(size)
        .with_min_inner_size([640.0, 440.0])
        .with_drag_and_drop(true);
    let result = eframe::run_native(
        "KYTTU - AI",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |cc| Ok(Box::new(ui::Studio::new(cc, config, notice)))),
    );
    // MCP servers are child processes; never leave them running after the window closes.
    core::mcp::shutdown_all();
    result
}

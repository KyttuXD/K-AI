#![cfg_attr(all(target_os = "windows", not(test)), windows_subsystem = "windows")]
mod core;
mod integrations;
mod knowledge;
mod ui;
use core::{config, engine, tools};
use knowledge::docs;

fn main() -> eframe::Result {
    eframe::run_native(
        "KYTTU - AI",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                // Logical points; the OS display scale multiplies these, so keep
                // them small enough to fit a 1366x768 screen at 125%.
                .with_inner_size([1100.0, 720.0])
                .with_min_inner_size([640.0, 440.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(ui::Studio::new(cc)))),
    )
}

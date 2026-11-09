//! Phosphor icons (regular weight) for labels and buttons. Text stays next to an icon; icon-only buttons get hover
//! text. Constants are plain `&str` glyphs, so they work anywhere text does.
#![allow(dead_code)] // The list is the shared vocabulary; not every glyph is used yet.
use eframe::egui;
use egui_phosphor::regular as ph;

pub const CHAT: &str = ph::CHAT_CIRCLE;
pub const LIBRARY: &str = ph::BOOKS;
pub const MODELS: &str = ph::CUBE;
pub const WORKBENCH: &str = ph::WRENCH;
pub const CHARACTERS: &str = ph::MASK_HAPPY;
pub const CONNECTIONS: &str = ph::PLUGS_CONNECTED;
pub const SETTINGS: &str = ph::GEAR;
pub const NEW_CHAT: &str = ph::NOTE_PENCIL;
pub const OPEN_FOLDER: &str = ph::FOLDER_OPEN;
pub const COPY: &str = ph::COPY;
pub const EDIT: &str = ph::PENCIL_SIMPLE;
pub const REGENERATE: &str = ph::ARROW_CLOCKWISE;
pub const SPEAK: &str = ph::SPEAKER_HIGH;
pub const STOP: &str = ph::STOP;
pub const SEND: &str = ph::PAPER_PLANE_RIGHT;
pub const ATTACH: &str = ph::PAPERCLIP;
pub const TRASH: &str = ph::TRASH;
pub const UNDO: &str = ph::ARROW_COUNTER_CLOCKWISE;
pub const PLUS: &str = ph::PLUS;
pub const DOWNLOAD: &str = ph::DOWNLOAD_SIMPLE;
pub const CHECK: &str = ph::CHECK;
pub const WARNING: &str = ph::WARNING;
pub const ERROR: &str = ph::X_CIRCLE;
pub const GIT: &str = ph::GIT_BRANCH;
pub const TERMINAL: &str = ph::TERMINAL_WINDOW;
pub const EDITOR: &str = ph::CODE;
pub const SEARCH: &str = ph::MAGNIFYING_GLASS;
pub const REFRESH: &str = ph::ARROWS_CLOCKWISE;
pub const PIN: &str = ph::PUSH_PIN;
pub const FORK: &str = ph::GIT_FORK;
pub const EXPORT: &str = ph::EXPORT;
pub const IMPORT: &str = ph::FILE_ARROW_DOWN;
pub const MORE: &str = ph::DOTS_THREE;
pub const CLOSE: &str = ph::X;
pub const EYE: &str = ph::EYE;
pub const SAVE: &str = ph::FLOPPY_DISK;

/// Button or menu text with an icon in front.
pub fn label(icon: &str, text: &str) -> String {
    format!("{icon}  {text}")
}

/// Adds the Phosphor font as the last fallback of both families. The existing fonts (egui's defaults and the OS
/// fallbacks of `install_fonts`) stay as they are. Call it after `install_fonts`, once at startup.
pub fn install(ctx: &egui::Context) {
    let stock = egui::FontDefinitions::default();
    let mut with_icons = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut with_icons, egui_phosphor::Variant::Regular);
    for (name, data) in with_icons.font_data {
        if stock.font_data.contains_key(&name) {
            continue;
        }
        ctx.add_font(egui::epaint::text::FontInsert {
            name,
            data: (*data).clone(),
            families: [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                .into_iter()
                .map(|family| egui::epaint::text::InsertFontFamily {
                    family,
                    priority: egui::epaint::text::FontPriority::Lowest,
                })
                .collect(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_puts_icon_first() {
        assert_eq!(label("X", "Delete"), "X  Delete");
    }

    #[test]
    fn glyphs_are_single_chars() {
        for g in [CHAT, TRASH, UNDO, SAVE, CLOSE, MORE, IMPORT] {
            assert_eq!(g.chars().count(), 1);
        }
    }
}

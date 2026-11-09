//! Every prompt text the app sends. Defaults are asset files embedded with `include_str!`;
//! a non-empty copy in `<data dir>/prompts/` wins. No prompt sentence lives in Rust code.
use super::config::{self, Config};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// Longest prompt text the editor saves (and the longest one read back), in characters.
pub const MAX_PROMPT: usize = 8000;
/// Largest part of a project `SOUL.md` that is read, in bytes.
pub const MAX_SOUL_FILE: usize = 8 * 1024;
/// Longest SOUL (global rules plus project rules) that reaches the prompt, in characters.
pub const MAX_SOUL: usize = 4000;
pub const MAX_SUGGESTIONS: usize = 20;
const MAX_SUGGESTION_TITLE: usize = 60;
const MAX_SUGGESTION_PROMPT: usize = 2000;
const SUGGESTIONS_FILE: &str = "suggestions.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptId {
    System,
    ModeChat,
    ModeCode,
    ModeHost,
    Sources,
    SoulFrame,
    SoulReminder,
    Role,
    Soul,
    SoulProject,
    Lore,
}
impl PromptId {
    pub const ALL: [PromptId; 11] = [
        PromptId::System,
        PromptId::ModeChat,
        PromptId::ModeCode,
        PromptId::ModeHost,
        PromptId::Sources,
        PromptId::SoulFrame,
        PromptId::SoulReminder,
        PromptId::Role,
        PromptId::Soul,
        PromptId::SoulProject,
        PromptId::Lore,
    ];
    pub fn label(self) -> &'static str {
        match self {
            PromptId::System => "System",
            PromptId::ModeChat => "Chat mode",
            PromptId::ModeCode => "Code mode",
            PromptId::ModeHost => "Discord tools",
            PromptId::Sources => "Sources",
            PromptId::SoulFrame => "Core rules frame",
            PromptId::SoulReminder => "Core rules reminder",
            PromptId::Role => "Character role",
            PromptId::Soul => "Core rules (SOUL)",
            PromptId::SoulProject => "Project rules frame",
            PromptId::Lore => "Lore frame",
        }
    }
    /// File name inside `<data dir>/prompts/`.
    pub fn file(self) -> &'static str {
        match self {
            PromptId::System => "system.md",
            PromptId::ModeChat => "mode_chat.md",
            PromptId::ModeCode => "mode_code.md",
            PromptId::ModeHost => "mode_host.md",
            PromptId::Sources => "sources.md",
            PromptId::SoulFrame => "soul_frame.md",
            PromptId::SoulReminder => "soul_reminder.md",
            PromptId::Role => "role.md",
            PromptId::Soul => "soul.md",
            PromptId::SoulProject => "soul_project.md",
            PromptId::Lore => "lore.md",
        }
    }
}

pub fn default_text(id: PromptId) -> &'static str {
    match id {
        PromptId::System => include_str!("../../assets/prompts/system.md"),
        PromptId::ModeChat => include_str!("../../assets/prompts/mode_chat.md"),
        PromptId::ModeCode => include_str!("../../assets/prompts/mode_code.md"),
        PromptId::ModeHost => include_str!("../../assets/prompts/mode_host.md"),
        PromptId::Sources => include_str!("../../assets/prompts/sources.md"),
        PromptId::SoulFrame => include_str!("../../assets/prompts/soul_frame.md"),
        PromptId::SoulReminder => include_str!("../../assets/prompts/soul_reminder.md"),
        PromptId::Role => include_str!("../../assets/prompts/role.md"),
        PromptId::Soul => include_str!("../../assets/prompts/soul.md"),
        PromptId::SoulProject => include_str!("../../assets/prompts/soul_project.md"),
        PromptId::Lore => include_str!("../../assets/prompts/lore.md"),
    }
}
const DEFAULT_SUGGESTIONS: &str = include_str!("../../assets/prompts/suggestions.json");

fn dir_in(base: &Path) -> PathBuf {
    base.join("prompts")
}
fn path_in(base: &Path, id: PromptId) -> PathBuf {
    dir_in(base).join(id.file())
}

/// The user's copy under `base`, if it exists and is not blank.
fn override_in(base: &Path, id: PromptId) -> Option<String> {
    let text = std::fs::read_to_string(path_in(base, id)).ok()?;
    (!text.trim().is_empty()).then(|| text.chars().take(MAX_PROMPT).collect())
}
fn text_in(base: &Path, id: PromptId) -> String {
    override_in(base, id).unwrap_or_else(|| default_text(id).to_string())
}
fn save_in(base: &Path, id: PromptId, text: &str) -> Result<()> {
    ensure!(
        text.chars().count() <= MAX_PROMPT,
        "A prompt takes at most {MAX_PROMPT} characters"
    );
    config::write_atomic(&path_in(base, id), text.as_bytes())
}
fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("Could not reset to the default"),
    }
}

/// The prompt as used: the data-folder copy if it exists and is not empty, else the default.
pub fn text(id: PromptId) -> String {
    text_in(&config::data_dir(), id)
}
pub fn is_custom(id: PromptId) -> bool {
    override_in(&config::data_dir(), id).is_some()
}
pub fn save(id: PromptId, text: &str) -> Result<()> {
    save_in(&config::data_dir(), id, text)
}
/// Deletes the user's copy, so the default applies again.
pub fn reset(id: PromptId) -> Result<()> {
    remove_if_present(&path_in(&config::data_dir(), id))
}

/// Fills `{{key}}` placeholders from `vars`; unknown placeholders are removed. A section
/// `{{?key}}...{{/key}}` is kept only when `key` has a non-blank value. Values are inserted
/// as they are and never expanded again.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let value = |key: &str| -> String {
        vars.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| (*v).to_string())
            .unwrap_or_default()
    };
    substitute(&sections(template, &value), &value)
}
fn sections(template: &str, value: &impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{?") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else { break };
        let key = after[..end].trim();
        let body = &after[end + 2..];
        let close = format!("{{{{/{key}}}}}");
        let Some(close_at) = body.find(&close) else {
            break;
        };
        out.push_str(&rest[..start]);
        if !value(key).trim().is_empty() {
            out.push_str(&sections(&body[..close_at], value));
        }
        rest = &body[close_at + close.len()..];
    }
    out.push_str(rest);
    out
}
fn substitute(template: &str, value: &impl Fn(&str) -> String) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&value(rest[start + 2..start + 2 + end].trim()));
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    out
}

/// A clickable starter prompt on the empty chat screen.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Suggestion {
    pub title: String,
    pub prompt: String,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SuggestionFile {
    chat: Vec<Suggestion>,
    code: Vec<Suggestion>,
}
fn suggestions_in(base: &Path) -> SuggestionFile {
    let user = std::fs::read_to_string(dir_in(base).join(SUGGESTIONS_FILE))
        .ok()
        .and_then(|s| serde_json::from_str::<SuggestionFile>(&s).ok());
    user.unwrap_or_else(|| serde_json::from_str(DEFAULT_SUGGESTIONS).unwrap_or_default())
}
fn clean(list: &[Suggestion]) -> Vec<Suggestion> {
    list.iter()
        .map(|s| Suggestion {
            title: s.title.trim().chars().take(MAX_SUGGESTION_TITLE).collect(),
            prompt: s
                .prompt
                .trim()
                .chars()
                .take(MAX_SUGGESTION_PROMPT)
                .collect(),
        })
        .filter(|s| !s.title.is_empty() && !s.prompt.is_empty())
        .take(MAX_SUGGESTIONS)
        .collect()
}
/// Starter prompts for Code mode (`coding`) or Chat mode.
pub fn suggestions(coding: bool) -> Vec<Suggestion> {
    let file = suggestions_in(&config::data_dir());
    clean(if coding { &file.code } else { &file.chat })
}
fn save_suggestions_in(base: &Path, chat: &[Suggestion], code: &[Suggestion]) -> Result<()> {
    let file = SuggestionFile {
        chat: clean(chat),
        code: clean(code),
    };
    config::save_json(&dir_in(base).join(SUGGESTIONS_FILE), &file)
}
pub fn save_suggestions(chat: &[Suggestion], code: &[Suggestion]) -> Result<()> {
    save_suggestions_in(&config::data_dir(), chat, code)
}
pub fn reset_suggestions() -> Result<()> {
    remove_if_present(&dir_in(&config::data_dir()).join(SUGGESTIONS_FILE))
}

/// A project's `SOUL.md`: a regular file directly inside the workspace (never a symlink),
/// UTF-8, at most `MAX_SOUL_FILE` bytes read.
fn project_soul(workspace: &Path) -> Option<String> {
    let path = workspace.join("SOUL.md");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    if path.canonicalize().ok()?.parent()? != workspace.canonicalize().ok()? {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .ok()?
        .take(MAX_SOUL_FILE as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => {
            // A cut in the middle of a character is fine; anything else is not UTF-8.
            let cut = e.utf8_error();
            if cut.error_len().is_some() {
                return None;
            }
            let bytes = e.into_bytes();
            String::from_utf8(bytes[..cut.valid_up_to()].to_vec()).ok()?
        }
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
/// Core rules read under `base`, plus the project's rules when a workspace is given.
pub(crate) fn soul_in(base: &Path, workspace: Option<&Path>, enabled: bool) -> Option<String> {
    if !enabled {
        return None;
    }
    let mut soul = text_in(base, PromptId::Soul).trim().to_string();
    if let Some(project) = workspace.and_then(project_soul) {
        let framed = render(
            &text_in(base, PromptId::SoulProject),
            &[("project", &project)],
        );
        soul.push_str("\n\n");
        soul.push_str(framed.trim());
    }
    let soul: String = soul.trim().chars().take(MAX_SOUL).collect();
    let soul = soul.trim_end();
    (!soul.is_empty()).then(|| soul.to_string())
}
/// The lore frame for the user turn, with the matching lore filled in.
pub fn lore_text(lore: &str) -> String {
    render(
        &text_in(&config::data_dir(), PromptId::Lore),
        &[("lore", lore)],
    )
    .trim()
    .to_string()
}
/// The SOUL: the global core rules plus the workspace's `SOUL.md`. `None` when SOUL is off.
pub fn soul_text(workspace: &Path, config: &Config) -> Option<String> {
    soul_in(&config::data_dir(), Some(workspace), config.soul_enabled)
}
/// The SOUL without any project file (chat and Discord never read workspace files).
pub fn soul_text_global(config: &Config) -> Option<String> {
    soul_in(&config::data_dir(), None, config.soul_enabled)
}

pub struct PromptParts<'a> {
    pub model: &'a str,
    /// Some only in Code mode without a host.
    pub workspace: Option<&'a Path>,
    pub coding: bool,
    pub host: bool,
    pub docs_or_web: bool,
    /// (name, notes, memory): notes are free text, memory is one note per line.
    pub character: Option<(&'a str, &'a str, &'a str)>,
    pub soul: Option<String>,
}

/// The system prompt: system, mode note, sources, SOUL frame, character role, SOUL reminder
/// (always last). It depends only on settings, never on the message, so the bytes are
/// identical between turns and the server reuses its prompt cache.
pub fn assemble(parts: &PromptParts) -> String {
    assemble_in(&config::data_dir(), parts)
}
/// [`assemble`] reading user prompt copies from `base` instead of the data folder.
pub fn assemble_in(base: &Path, parts: &PromptParts) -> String {
    let mut s = render(
        &text_in(base, PromptId::System),
        &[("model", parts.model.trim())],
    )
    .trim_end()
    .to_string();
    s.push('\n');
    let mode = if parts.host {
        PromptId::ModeHost
    } else if parts.coding {
        PromptId::ModeCode
    } else {
        PromptId::ModeChat
    };
    let workspace = parts
        .workspace
        .filter(|_| parts.coding && !parts.host)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    s.push_str(render(&text_in(base, mode), &[("workspace", &workspace)]).trim());
    if parts.coding || parts.docs_or_web {
        s.push('\n');
        s.push_str(text_in(base, PromptId::Sources).trim());
    }
    let soul = parts
        .soul
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if let Some(soul) = soul {
        s.push_str("\n\n");
        s.push_str(render(&text_in(base, PromptId::SoulFrame), &[("soul", soul)]).trim());
    }
    if let Some((name, notes, memory)) = parts.character {
        let vars = [
            ("name", name.trim()),
            ("notes", notes.trim()),
            ("memory", memory.trim()),
        ];
        s.push_str("\n\n");
        s.push_str(render(&text_in(base, PromptId::Role), &vars).trim());
    }
    if soul.is_some() {
        s.push_str("\n\n");
        s.push_str(text_in(base, PromptId::SoulReminder).trim());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts<'a>() -> PromptParts<'a> {
        PromptParts {
            model: "m1",
            workspace: None,
            coding: false,
            host: false,
            docs_or_web: false,
            character: None,
            soul: None,
        }
    }
    fn write(base: &Path, id: PromptId, text: &str) {
        std::fs::create_dir_all(dir_in(base)).unwrap();
        std::fs::write(path_in(base, id), text).unwrap();
    }

    #[test]
    fn render_fills_known_removes_unknown_and_never_reexpands() {
        assert_eq!(
            render("a {{x}} b {{ y }} c{{z}}", &[("x", "1"), ("y", "2")]),
            "a 1 b 2 c"
        );
        assert_eq!(render("{{x}}", &[("x", "{{y}}"), ("y", "no")]), "{{y}}");
        assert_eq!(render("open {{ never closed", &[]), "open {{ never closed");
        let t = "r{{?n}}\nnotes: {{n}}{{/n}}{{?m}}\nmem{{/m}}.";
        assert_eq!(render(t, &[("n", "hi")]), "r\nnotes: hi.");
        assert_eq!(render(t, &[("n", " "), ("m", "x")]), "r\nmem.");
        assert_eq!(render(t, &[]), "r.");
    }

    #[test]
    fn defaults_are_present_and_render_cleanly() {
        for id in PromptId::ALL {
            assert!(!default_text(id).trim().is_empty(), "{}", id.file());
        }
        let both: SuggestionFile = serde_json::from_str(DEFAULT_SUGGESTIONS).unwrap();
        assert!(!both.chat.is_empty() && !both.code.is_empty());
        let role = render(
            default_text(PromptId::Role),
            &[("name", "Ada"), ("notes", ""), ("memory", "- a")],
        );
        assert!(role.contains("\"Ada\"") && !role.contains("Character notes"));
        assert!(role.contains("Character memory:\n- a") && !role.contains("{{"));
    }

    #[test]
    fn override_wins_blank_falls_back_and_reset_restores() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let id = PromptId::Sources;
        assert_eq!(text_in(base, id), default_text(id));
        save_in(base, id, "mine").unwrap();
        assert_eq!(text_in(base, id), "mine");
        assert!(override_in(base, id).is_some());
        write(base, id, "  \n");
        assert_eq!(text_in(base, id), default_text(id));
        save_in(base, id, "mine").unwrap();
        remove_if_present(&path_in(base, id)).unwrap();
        remove_if_present(&path_in(base, id)).unwrap();
        assert!(override_in(base, id).is_none());
        assert!(save_in(base, PromptId::Soul, &"x".repeat(MAX_PROMPT + 1)).is_err());
    }

    #[test]
    fn assembly_orders_sections_and_ends_with_the_reminder() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let ws = Path::new("/work/space");
        let p = PromptParts {
            workspace: Some(ws),
            coding: true,
            character: Some(("Ada", "Short sentences.", "- likes tea")),
            soul: Some("RULE-ONE".into()),
            ..parts()
        };
        let s = assemble_in(base, &p);
        assert!(s.starts_with("You are KYTTU"));
        let at = |needle: &str| s.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
        let (system, mode, sources, soul, role, notes, mem) = (
            at("Do not repeat"),
            at("Workspace folder:"),
            at("reference data"),
            at("RULE-ONE"),
            at("speak as the character \"Ada\""),
            at("Short sentences."),
            at("- likes tea"),
        );
        assert!(system < mode && mode < sources && sources < soul && soul < role);
        assert!(role < notes && notes < mem);
        assert!(s.contains("m1") && s.contains(&ws.display().to_string()));
        assert!(s.ends_with(default_text(PromptId::SoulReminder).trim()));
        assert_eq!(s, assemble_in(base, &p), "same input, same bytes");
    }

    #[test]
    fn assembly_variants() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        // Chat: no workspace, no sources, no soul, no role.
        let chat = assemble_in(
            base,
            &PromptParts {
                workspace: Some(Path::new("/secret/path")),
                ..parts()
            },
        );
        assert!(chat.contains(default_text(PromptId::ModeChat).trim()));
        assert!(!chat.contains("/secret/path") && !chat.contains("reference data"));
        assert!(!chat.contains("Core rules") && !chat.contains("Reminder"));
        // Sources appear with documentation or web.
        let docs = assemble_in(
            base,
            &PromptParts {
                docs_or_web: true,
                ..parts()
            },
        );
        assert!(docs.contains("reference data"));
        // Host beats code and shows no workspace.
        let host = assemble_in(
            base,
            &PromptParts {
                host: true,
                coding: true,
                workspace: Some(Path::new("/secret/path")),
                ..parts()
            },
        );
        assert!(host.contains(default_text(PromptId::ModeHost).trim()));
        assert!(!host.contains("/secret/path"));
        // A character without a soul has no reminder; a soul alone has no role.
        let role = assemble_in(
            base,
            &PromptParts {
                character: Some(("Bo", "", "")),
                ..parts()
            },
        );
        assert!(role.contains("\"Bo\"") && !role.contains("Reminder"));
        let soul = assemble_in(
            base,
            &PromptParts {
                soul: Some("R".into()),
                ..parts()
            },
        );
        assert!(!soul.contains("speak as") && soul.contains("Reminder"));
        // Overrides reach the assembled text.
        write(base, PromptId::ModeChat, "CUSTOM CHAT {{model}}");
        write(base, PromptId::System, "SYS {{model}}");
        assert_eq!(assemble_in(base, &parts()), "SYS m1\nCUSTOM CHAT");
    }

    #[test]
    fn soul_is_capped_and_can_be_switched_off() {
        let dir = tempfile::tempdir().unwrap();
        let (base, ws) = (dir.path().join("data"), dir.path().join("ws"));
        std::fs::create_dir_all(&ws).unwrap();
        assert!(soul_in(&base, Some(&ws), false).is_none());
        let plain = soul_in(&base, Some(&ws), true).unwrap();
        assert_eq!(plain, default_text(PromptId::Soul).trim());
        std::fs::write(ws.join("SOUL.md"), "é".repeat(20_000)).unwrap();
        let big = soul_in(&base, Some(&ws), true).unwrap();
        assert!(big.chars().count() <= MAX_SOUL);
        assert!(big.starts_with(plain.as_str()) && big.contains("Project rules:"));
        // Without a workspace the project file is ignored.
        assert_eq!(soul_in(&base, None, true).unwrap(), plain);
        // Only the first 8 KB are read; a cut inside a character is tolerated.
        assert!(project_soul(&ws).unwrap().len() <= MAX_SOUL_FILE);
        // Not UTF-8: ignored.
        std::fs::write(ws.join("SOUL.md"), [0xff, 0xfe, b'a']).unwrap();
        assert!(project_soul(&ws).is_none());
        // A folder named SOUL.md is not a file.
        std::fs::remove_file(ws.join("SOUL.md")).unwrap();
        std::fs::create_dir(ws.join("SOUL.md")).unwrap();
        assert!(project_soul(&ws).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn project_soul_symlink_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (ws, outside) = (dir.path().join("ws"), dir.path().join("secret.md"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(&outside, "leak").unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("SOUL.md")).unwrap();
        assert!(project_soul(&ws).is_none());
        let inside = ws.join("real.md");
        std::fs::write(&inside, "ok").unwrap();
        std::fs::remove_file(ws.join("SOUL.md")).unwrap();
        std::os::unix::fs::symlink(&inside, ws.join("SOUL.md")).unwrap();
        assert!(project_soul(&ws).is_none(), "any symlink is refused");
    }

    #[test]
    fn suggestions_default_override_and_cleaning() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let s = |t: &str, p: &str| Suggestion {
            title: t.into(),
            prompt: p.into(),
        };
        assert!(!suggestions_in(base).chat.is_empty());
        save_suggestions_in(base, &[s(" A ", " go "), s("", "x")], &[]).unwrap();
        let file = suggestions_in(base);
        assert_eq!(file.chat, vec![s("A", "go")]);
        assert!(file.code.is_empty());
        std::fs::write(dir_in(base).join(SUGGESTIONS_FILE), "not json").unwrap();
        assert!(
            !suggestions_in(base).code.is_empty(),
            "broken override falls back"
        );
    }
}

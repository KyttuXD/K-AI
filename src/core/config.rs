use crate::integrations::discord::BotProfile;
use anyhow::Result;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub docs: PathBuf,
    pub workspace: PathBuf,
    pub model: String,
    pub embedding_model: String,
    pub personas: Vec<super::personas::Persona>,
    /// Pre-v3 single Discord bot; kept so old files parse. See `discord_bots`.
    pub discord_channel: String,
    pub discord_prefix: String,
    pub discord_docs: bool,
    pub discord_group: bool,
    pub direct_backend: bool,
    pub direct_url: String,
    pub context_tokens: usize,
    pub response_tokens: usize,
    /// Built-in llama.cpp runtime.
    pub gguf_model: PathBuf,
    pub llama_server: PathBuf,
    pub llama_port: u16,
    pub llama_threads: usize,
    /// None = automatic (CPU-only on Intel Macs, server default elsewhere).
    pub llama_gpu_layers: Option<i32>,
    pub ui_scale: f32,
    pub show_inspector: bool,
    /// Chat options, remembered across launches.
    pub coding: bool,
    pub use_docs: bool,
    pub use_web: bool,
    pub use_embeddings: bool,
    pub group_chat: bool,
    pub pack: String,
    pub active_persona: usize,
    /// "dark", "light" or "system".
    pub theme: String,
    /// Logical points (before the interface scale), restored at launch.
    pub window_size: Option<[f32; 2]>,
    pub window_pos: Option<[f32; 2]>,
    pub sidebar_collapsed: bool,
    pub max_steps: usize,
    pub temperature: f32,
    pub top_p: f32,
    pub repeat_penalty: f32,
    /// Automatic first-run download of llama-server and a model ran once.
    pub setup_attempted: bool,
    /// File name of the `setup::MODELS` entry to download; empty = recommended.
    pub setup_model: String,
    pub auto_update: bool,
    /// Unix seconds of the last update check.
    pub last_update_check: u64,
    pub discord_bots: Vec<BotProfile>,
    /// Id of the open conversation under data_dir()/conversations.
    pub active_conversation: String,
    /// Bumped when defaults change in a way existing settings should adopt.
    /// Missing in older files, so it defaults to 0 there (not the current version).
    #[serde(default)]
    pub settings_version: u32,
}
const SETTINGS_VERSION: u32 = 3;
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("KyttuAI")
}
impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            docs: home.join("Documents/Projects/Docs"),
            workspace: std::env::current_dir().unwrap_or_else(|_| home.clone()),
            model: "qwen2.5-coder:3b".into(),
            embedding_model: "nomic-embed-text:latest".into(),
            personas: vec![],
            discord_channel: String::new(),
            discord_prefix: "!ai".into(),
            discord_docs: false,
            discord_group: false,
            // llama.cpp is the primary runtime on every platform; Ollama is optional.
            direct_backend: true,
            direct_url: "http://127.0.0.1:8080".into(),
            context_tokens: 8192,
            response_tokens: 1536,
            gguf_model: PathBuf::new(),
            llama_server: PathBuf::new(),
            llama_port: 8080,
            llama_threads: 0,
            llama_gpu_layers: None,
            ui_scale: 1.0,
            show_inspector: false,
            coding: true,
            use_docs: true,
            use_web: false,
            use_embeddings: false,
            group_chat: false,
            pack: String::new(),
            active_persona: 0,
            theme: "dark".into(),
            window_size: None,
            window_pos: None,
            sidebar_collapsed: false,
            max_steps: 12,
            temperature: 0.2,
            top_p: 0.95,
            repeat_penalty: 1.1,
            setup_attempted: false,
            setup_model: String::new(),
            auto_update: true,
            last_update_check: 0,
            discord_bots: vec![],
            active_conversation: String::new(),
            settings_version: SETTINGS_VERSION,
        }
    }
}
impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("settings.json")
    }
    /// Loads settings; the message explains a recovery from a damaged file.
    pub fn load_checked() -> (Self, Option<String>) {
        let (config, notice) = read_json::<Self>(&Self::path());
        (config.map(Self::migrate).unwrap_or_default(), notice)
    }
    fn migrate(mut self) -> Self {
        if self.settings_version < 2 {
            // v2: llama.cpp became the primary provider.
            self.direct_backend = true;
        }
        if self.settings_version < 3
            && self.discord_bots.is_empty()
            && !self.discord_channel.trim().is_empty()
        {
            // v3: several Discord bots, each with its own channels.
            let defaults = BotProfile::default();
            self.discord_bots.push(BotProfile {
                channels: vec![self.discord_channel.trim().to_owned()],
                prefix: if self.discord_prefix.trim().is_empty() {
                    defaults.prefix.clone()
                } else {
                    self.discord_prefix.trim().to_owned()
                },
                group: self.discord_group,
                docs: self.discord_docs,
                ..defaults
            });
        }
        self.settings_version = SETTINGS_VERSION;
        self
    }
    pub fn save(&self) -> Result<()> {
        save_json(&Self::path(), self)
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes JSON through a temp file and a rename, keeping the previous file as `.bak`.
pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = sibling(path, ".tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
    }
    if path.is_file() {
        std::fs::rename(path, sibling(path, ".bak"))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads JSON written by [`save_json`]. A file that does not parse is kept as
/// `.corrupt`, and the `.bak` copy is restored and returned with a message.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> (Option<T>, Option<String>) {
    let parse = |p: &Path| -> Option<T> {
        std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    };
    let backup = sibling(path, ".bak");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match std::fs::read(path) {
        Err(_) => (parse(&backup), None), // missing, or interrupted between renames
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => (Some(value), None),
            Err(e) => {
                let kept = sibling(path, ".corrupt");
                let _ = std::fs::write(&kept, &bytes);
                match parse(&backup) {
                    Some(value) => {
                        let _ = std::fs::copy(&backup, path);
                        (
                            Some(value),
                            Some(format!(
                                "{name} could not be read ({e}). The backup was restored; the damaged file was kept as {}.",
                                kept.display()
                            )),
                        )
                    }
                    None => (
                        None,
                        Some(format!(
                            "{name} could not be read ({e}) and no usable backup exists. Defaults are in use; the damaged file was kept as {}.",
                            kept.display()
                        )),
                    ),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_install_has_no_preset_characters_or_external_sharing() {
        let config = Config::default();
        assert!(config.personas.is_empty());
        assert!(!config.discord_docs);
        assert!(config.discord_channel.is_empty());
        assert!(config.discord_bots.is_empty());
        assert!(config.auto_update && !config.setup_attempted);
        assert_eq!(config.theme, "dark");
    }
    #[test]
    fn llama_cpp_is_primary_for_new_and_old_settings() {
        assert!(Config::default().direct_backend);
        let old: Config = serde_json::from_str(r#"{"direct_backend":false,"model":"x"}"#).unwrap();
        assert_eq!(old.settings_version, 0);
        assert!(old.migrate().direct_backend);
        // A user who picks Ollama after migrating keeps it.
        let current: Config =
            serde_json::from_str(r#"{"direct_backend":false,"settings_version":2}"#).unwrap();
        assert!(!current.migrate().direct_backend);
    }
    #[test]
    fn old_discord_fields_become_one_bot_profile() {
        let old: Config = serde_json::from_str(
            r#"{"settings_version":2,"discord_channel":" 123 ","discord_prefix":"!k","discord_group":true}"#,
        )
        .unwrap();
        let migrated = old.migrate();
        assert_eq!(migrated.settings_version, 3);
        assert_eq!(migrated.discord_bots.len(), 1);
        let bot = &migrated.discord_bots[0];
        assert_eq!(bot.channels, vec!["123".to_string()]);
        assert_eq!(bot.prefix, "!k");
        assert!(bot.group && !bot.docs);
        // Once at v3, removing every bot is respected.
        let current: Config =
            serde_json::from_str(r#"{"settings_version":3,"discord_channel":"123"}"#).unwrap();
        assert!(current.migrate().discord_bots.is_empty());
    }
    #[test]
    fn new_fields_default_in_old_files() {
        let old: Config = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert!(old.coding && old.use_docs && !old.use_web);
        assert_eq!(old.max_steps, 12);
        assert!(old.window_size.is_none());
    }
    #[test]
    fn atomic_save_keeps_backup_and_recovers_from_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save_json(&path, &vec![1]).unwrap();
        save_json(&path, &vec![2]).unwrap();
        assert_eq!(read_json::<Vec<i32>>(&path), (Some(vec![2]), None));
        std::fs::write(&path, b"{ broken").unwrap();
        let (value, notice) = read_json::<Vec<i32>>(&path);
        assert_eq!(value, Some(vec![1]));
        assert!(notice.unwrap().contains("backup was restored"));
        assert_eq!(
            std::fs::read(sibling(&path, ".corrupt")).unwrap(),
            b"{ broken"
        );
        // The restored file is valid again, so the next save keeps a good backup.
        assert_eq!(read_json::<Vec<i32>>(&path), (Some(vec![1]), None));
        // Interrupted between the two renames: only the backup exists.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_json::<Vec<i32>>(&path).0, Some(vec![1]));
        assert_eq!(
            read_json::<Vec<i32>>(&dir.path().join("missing.json")),
            (None, None)
        );
    }
}

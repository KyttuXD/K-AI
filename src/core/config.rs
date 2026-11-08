use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub docs: PathBuf,
    pub workspace: PathBuf,
    pub model: String,
    pub embedding_model: String,
    pub personas: Vec<super::personas::Persona>,
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
    /// Bumped when defaults change in a way existing settings should adopt.
    /// Missing in older files, so it defaults to 0 there (not the current version).
    #[serde(default)]
    pub settings_version: u32,
}
const SETTINGS_VERSION: u32 = 2;
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
            settings_version: SETTINGS_VERSION,
        }
    }
}
impl Config {
    pub fn load() -> Self {
        std::fs::read(data_dir().join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .map(Self::migrate)
            .unwrap_or_default()
    }
    fn migrate(mut self) -> Self {
        if self.settings_version < 2 {
            // v2: llama.cpp became the primary provider.
            self.direct_backend = true;
        }
        self.settings_version = SETTINGS_VERSION;
        self
    }
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        std::fs::write(
            data_dir().join("settings.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        Ok(())
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
}

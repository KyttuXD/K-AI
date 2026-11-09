use crate::integrations::discord::BotProfile;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Pre-v4 single documentation folder; kept so old files parse. See `doc_sources`.
    pub docs: PathBuf,
    pub doc_sources: Vec<crate::knowledge::docs::DocSource>,
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
    /// The user toggled Embedding rerank, so it is no longer turned on automatically.
    #[serde(default)]
    pub rerank_user_set: bool,
    pub group_chat: bool,
    /// Name of the documentation source to search; empty = all enabled sources.
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
    /// "llama" (a second llama-server with --embedding) or "ollama".
    pub embedding_backend: String,
    pub embedding_gguf: PathBuf,
    /// Per-model overrides, keyed by GGUF file name.
    pub model_presets: BTreeMap<String, crate::core::llama::ModelPreset>,
    pub low_end: bool,
    pub task_profiles: Vec<crate::core::terminal::TaskProfile>,
    pub mcp_servers: Vec<crate::core::mcp::McpServer>,
    /// Direct endpoints may leave this computer only when on and the host is trusted.
    pub allow_lan: bool,
    pub trusted_endpoints: Vec<String>,
    pub accent: [u8; 3],
    pub compact: bool,
    /// Recent workspaces, newest first.
    pub projects: Vec<PathBuf>,
    pub pinned_conversations: Vec<String>,
    pub last_view: usize,
    /// Window [x, y, width, height] in logical points, keyed by monitor size "WxH".
    pub monitor_windows: BTreeMap<String, [f32; 4]>,
    /// Read replies aloud with the OS voice.
    pub tts: bool,
    /// Extra environment variable names approved commands may see, per workspace path.
    pub workspace_env: BTreeMap<String, Vec<String>>,
    pub confirm_web_query: bool,
    /// Chat as the selected character; off = the plain agent ("No character").
    pub use_character: bool,
    /// Core rules (SOUL) are added to the prompt.
    pub soul_enabled: bool,
    /// SOUL also applies while no character is selected.
    pub soul_when_no_character: bool,
    /// "stable" or "nightly".
    pub update_channel: String,
}
const SETTINGS_VERSION: u32 = 5;
pub const DEFAULT_ACCENT: [u8; 3] = [247, 87, 108];
pub const MAX_PROJECTS: usize = 12;
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("KyttuAI")
}
impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            // No preset folder: every computer keeps its documentation somewhere else.
            docs: PathBuf::new(),
            doc_sources: vec![],
            workspace: std::env::current_dir().unwrap_or_else(|_| home.clone()),
            // The alias llama.rs gives the default download (its file stem).
            model: "Llama-3.2-1B-Instruct-Q4_K_M".into(),
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
            rerank_user_set: false,
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
            embedding_backend: "llama".into(),
            embedding_gguf: PathBuf::new(),
            model_presets: BTreeMap::new(),
            low_end: false,
            task_profiles: vec![],
            mcp_servers: vec![],
            allow_lan: false,
            trusted_endpoints: vec![],
            accent: DEFAULT_ACCENT,
            compact: false,
            projects: vec![],
            pinned_conversations: vec![],
            last_view: 0,
            monitor_windows: BTreeMap::new(),
            tts: false,
            workspace_env: BTreeMap::new(),
            confirm_web_query: true,
            use_character: false,
            soul_enabled: true,
            soul_when_no_character: false,
            update_channel: "stable".into(),
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
        if self.settings_version < 4 {
            // v4: documentation sources replace the single docs folder; packs are source names.
            if self.doc_sources.is_empty() {
                self.doc_sources = crate::knowledge::docs::migrate_docs_dir(&self.docs);
            }
            self.pack.clear();
        }
        if self.settings_version < 5 {
            // v5: "No character" exists; old installs always chatted as a character.
            self.use_character = !self.personas.is_empty();
            // Characters saved before the Available-on-Discord switch worked on Discord.
            for persona in &mut self.personas {
                persona.discord = true;
            }
        }
        self.settings_version = SETTINGS_VERSION;
        self
    }
    /// Checks what the UI can get wrong before it is written to disk.
    pub fn validate(&self) -> Result<()> {
        super::personas::validate(&self.personas)?;
        let mut names = HashSet::new();
        for server in &self.mcp_servers {
            ensure!(
                valid_name(&server.name, 32),
                "MCP server names must be 1–32 letters, digits, - or _"
            );
            ensure!(
                names.insert(server.name.to_lowercase()),
                "MCP server names must be unique"
            );
            ensure!(
                !server.enabled || !server.command.trim().is_empty(),
                "MCP server {} needs a command",
                server.name
            );
        }
        for host in &self.trusted_endpoints {
            ensure!(
                valid_host(host),
                "Trusted endpoint \"{host}\" must be a host name or IP address, without http:// or a path"
            );
        }
        for name in self.workspace_env.values().flatten() {
            ensure!(
                crate::core::tools::valid_env_name(name),
                "\"{name}\" is not a valid environment variable name"
            );
        }
        ensure!(
            self.projects.len() <= MAX_PROJECTS,
            "Too many recent projects"
        );
        Ok(())
    }
    /// Embedding rerank turns itself on once it can help: an embedding model is installed
    /// (`embed_installed`), at least one documentation source is enabled, and the user has
    /// never toggled it.
    pub fn rerank_should_auto_enable(&self, embed_installed: bool) -> bool {
        crate::knowledge::embeddings::should_auto_enable(
            self,
            self.rerank_user_set,
            embed_installed,
        )
    }
    /// Puts `path` first in the recent projects, keeping at most MAX_PROJECTS.
    pub fn remember_project(&mut self, path: &Path) {
        self.projects.retain(|p| p != path);
        self.projects.insert(0, path.to_path_buf());
        self.projects.truncate(MAX_PROJECTS);
    }
}

fn valid_name(name: &str, max: usize) -> bool {
    (1..=max).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_host(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Writes pretty JSON through a temp file and a rename, keeping the previous file as `.bak`.
pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
}
/// Like [`save_json`] without indentation, for large files such as conversations.
pub fn save_json_compact<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}
/// Replaces `path` with `bytes` through a temp file and a rename, keeping the previous file as `.bak`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = sibling(path, ".tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
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
        // No preset documentation folder; the automatic web query is confirmed by default.
        assert!(config.doc_sources.is_empty() && config.docs.as_os_str().is_empty());
        assert!(config.confirm_web_query && !config.allow_lan && !config.tts);
        assert_eq!(config.embedding_backend, "llama");
        assert_eq!(config.accent, DEFAULT_ACCENT);
        assert_eq!(config.settings_version, SETTINGS_VERSION);
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
        assert_eq!(migrated.settings_version, SETTINGS_VERSION);
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
    fn old_docs_folder_becomes_one_linked_source() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("MyDocs");
        std::fs::create_dir_all(&docs).unwrap();
        let json = serde_json::json!({"settings_version":3,"docs":docs,"pack":"rust"});
        let old: Config = serde_json::from_value(json).unwrap();
        let migrated = old.migrate();
        assert_eq!(migrated.settings_version, SETTINGS_VERSION);
        assert_eq!(migrated.doc_sources.len(), 1);
        let source = &migrated.doc_sources[0];
        assert_eq!(source.name, "MyDocs");
        assert_eq!(
            source.path.canonicalize().unwrap(),
            docs.canonicalize().unwrap()
        );
        assert_eq!(source.mode, crate::knowledge::docs::SourceMode::Linked);
        assert!(migrated.pack.is_empty());
        // A missing old folder adds nothing, and v4 files are left alone.
        let gone: Config =
            serde_json::from_str(r#"{"settings_version":3,"docs":"/no/such/dir"}"#).unwrap();
        assert!(gone.migrate().doc_sources.is_empty());
        let json = serde_json::json!({"settings_version":4,"docs":docs});
        let current: Config = serde_json::from_value(json).unwrap();
        assert!(current.migrate().doc_sources.is_empty());
    }
    #[test]
    fn validation_rejects_bad_values() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());
        config.trusted_endpoints = vec!["http://192.168.1.5/".into()];
        assert!(config.validate().is_err());
        config.trusted_endpoints = vec!["192.168.1.5".into(), "studio.local:8080".into()];
        assert!(config.validate().is_ok());
        config
            .workspace_env
            .insert("C:/w".into(), vec!["MY TOKEN".into()]);
        assert!(config.validate().is_err());
        config
            .workspace_env
            .insert("C:/w".into(), vec!["CARGO_HOME".into()]);
        assert!(config.validate().is_ok());
        config.mcp_servers = vec![
            crate::core::mcp::McpServer {
                name: "fs".into(),
                command: "npx".into(),
                ..Default::default()
            };
            2
        ];
        assert!(config.validate().is_err());
        config.mcp_servers.truncate(1);
        assert!(config.validate().is_ok());
        config.personas = vec![Default::default(), Default::default()];
        assert!(config.validate().is_err());
    }
    #[test]
    fn recent_projects_are_unique_and_capped() {
        let mut config = Config::default();
        for i in 0..20 {
            config.remember_project(Path::new(&format!("/p{i}")));
        }
        config.remember_project(Path::new("/p5"));
        assert_eq!(config.projects.len(), MAX_PROJECTS);
        assert_eq!(config.projects[0], PathBuf::from("/p5"));
        assert_eq!(
            config
                .projects
                .iter()
                .filter(|p| p.as_path() == Path::new("/p5"))
                .count(),
            1
        );
    }
    #[test]
    fn new_fields_default_in_old_files() {
        let old: Config = serde_json::from_str(r#"{"model":"x"}"#).unwrap();
        assert!(old.coding && old.use_docs && !old.use_web);
        assert_eq!(old.max_steps, 12);
        assert!(old.window_size.is_none());
        assert!(old.confirm_web_query && old.mcp_servers.is_empty());
        assert!(!old.rerank_user_set);
    }
    #[test]
    fn v5_migration_keeps_characters_on_and_enables_discord() {
        let old: Config = serde_json::from_str(
            r#"{"settings_version":4,"personas":[{"name":"A"},{"name":"B","discord":false}]}"#,
        )
        .unwrap();
        let migrated = old.migrate();
        assert!(migrated.use_character);
        assert!(migrated.personas.iter().all(|p| p.discord));
        assert!(migrated.soul_enabled && !migrated.soul_when_no_character);
        assert_eq!(migrated.update_channel, "stable");
        // No characters: the plain agent stays selected.
        let empty: Config = serde_json::from_str(r#"{"settings_version":4}"#).unwrap();
        assert!(!empty.migrate().use_character);
        // v5 files are left alone.
        let current: Config = serde_json::from_str(
            r#"{"settings_version":5,"personas":[{"name":"A","discord":false}]}"#,
        )
        .unwrap();
        let current = current.migrate();
        assert!(!current.use_character && !current.personas[0].discord);
    }
    #[test]
    fn rerank_auto_enables_only_when_useful_and_unset() {
        let mut config = Config::default();
        assert!(
            !config.rerank_should_auto_enable(true),
            "no documentation source"
        );
        config.doc_sources = vec![crate::knowledge::docs::DocSource {
            id: "a".into(),
            name: "Docs".into(),
            path: PathBuf::from("/docs"),
            mode: crate::knowledge::docs::SourceMode::Linked,
            enabled: true,
        }];
        assert!(config.rerank_should_auto_enable(true));
        assert!(
            !config.rerank_should_auto_enable(false),
            "no embedding model"
        );
        config.doc_sources[0].enabled = false;
        assert!(!config.rerank_should_auto_enable(true), "source disabled");
        config.doc_sources[0].enabled = true;
        config.rerank_user_set = true;
        assert!(!config.rerank_should_auto_enable(true), "the user chose");
        config.rerank_user_set = false;
        config.use_embeddings = true;
        assert!(!config.rerank_should_auto_enable(true), "already on");
    }
    #[test]
    fn atomic_save_keeps_backup_and_recovers_from_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        save_json(&path, &vec![1]).unwrap();
        save_json_compact(&path, &vec![2]).unwrap();
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

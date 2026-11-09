//! Per-bot Discord server data: auto-reply channels and server characters.
//!
//! Files live under `data_dir()/discord/<bot>/`:
//! - `auto_channels.json`: server ID → channel IDs where the bot answers without a prefix;
//! - `servers/<server>/characters.json`: characters imported by that server's admins.
//!
//! Every write goes through `config::save_json` (temp file, rename, `.bak`), and every read
//! is size-capped so a damaged or hostile file cannot exhaust memory. Server characters come
//! from Discord users, so they are always re-checked on load and never pick a model.
use super::discord::sanitize;
use crate::{
    config::{self, data_dir},
    core::personas::{self, LoreEntry, Persona},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

/// Limits for one server character.
pub const MAX_NAME: usize = 64;
pub const MAX_INSTRUCTIONS: usize = 4000;
pub const MAX_LORE_ENTRIES: usize = 50;
pub const MAX_LORE_TEXT: usize = 1000;
const MAX_LORE_KEYS: usize = 20;
const MAX_KEY: usize = 64;
const MAX_AVATAR: usize = 1000;
/// Hard ceilings for the per-bot settings (`server_char_max_kb`, `server_char_limit`).
pub const MAX_CHAR_KB: usize = 256;
pub const MAX_CHARS_PER_SERVER: usize = 50;
/// Auto-reply channels per server, and servers per bot.
pub const MAX_AUTO_PER_GUILD: usize = 25;
pub const MAX_AUTO_GUILDS: usize = 100;
/// Largest files read back from disk.
const AUTO_FILE_CAP: u64 = 256 * 1024;
const CHARS_FILE_CAP: u64 = 4 * 1024 * 1024;
const AUTO_FILE: &str = "auto_channels.json";
const CHARS_FILE: &str = "characters.json";

/// Serializes read-modify-write cycles (the UI and the bot threads share these files).
static IO: Mutex<()> = Mutex::new(());
fn io_lock() -> MutexGuard<'static, ()> {
    IO.lock().unwrap_or_else(|e| e.into_inner())
}

/// Auto-reply channels per server: server ID → channel IDs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct AutoChannels(pub BTreeMap<u64, BTreeSet<u64>>);
impl AutoChannels {
    pub fn contains(&self, guild: u64, channel: u64) -> bool {
        self.0.get(&guild).is_some_and(|c| c.contains(&channel))
    }
    /// Servers with at least one auto channel, ascending.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn guilds(&self) -> Vec<u64> {
        self.0.keys().copied().collect()
    }
    pub fn channels(&self, guild: u64) -> Vec<u64> {
        self.0
            .get(&guild)
            .map(|c| c.iter().copied().collect())
            .unwrap_or_default()
    }
    /// Turns a channel on or off; returns whether anything changed.
    pub fn set(&mut self, guild: u64, channel: u64, on: bool) -> Result<bool> {
        ensure!(
            guild > 0 && channel > 0,
            "Auto channels need a server channel"
        );
        if !on {
            let Some(set) = self.0.get_mut(&guild) else {
                return Ok(false);
            };
            let changed = set.remove(&channel);
            if set.is_empty() {
                self.0.remove(&guild);
            }
            return Ok(changed);
        }
        if self.contains(guild, channel) {
            return Ok(false);
        }
        ensure!(
            self.0.contains_key(&guild) || self.0.len() < MAX_AUTO_GUILDS,
            "Auto channels are on in {MAX_AUTO_GUILDS} servers already"
        );
        let set = self.0.entry(guild).or_default();
        ensure!(
            set.len() < MAX_AUTO_PER_GUILD,
            "At most {MAX_AUTO_PER_GUILD} auto channels per server"
        );
        set.insert(channel);
        Ok(true)
    }
    /// Removes a server's auto channels; returns whether it had any.
    pub fn remove_guild(&mut self, guild: u64) -> bool {
        self.0.remove(&guild).is_some()
    }
    /// Drops zero IDs and empty servers and enforces the caps (keeping the lowest IDs).
    fn normalize(&mut self) {
        self.0.remove(&0);
        for set in self.0.values_mut() {
            set.remove(&0);
            while set.len() > MAX_AUTO_PER_GUILD {
                set.pop_last();
            }
        }
        self.0.retain(|_, set| !set.is_empty());
        while self.0.len() > MAX_AUTO_GUILDS {
            self.0.pop_last();
        }
    }
}

/// Import limits for server characters: the bot's `server_char_max_kb` and
/// `server_char_limit`, plus names that are already taken outside the server list
/// (for example the bot's own Discord characters), so `@Name` stays unambiguous.
#[derive(Clone, Debug)]
pub struct ImportLimits {
    pub max_kb: usize,
    pub max_count: usize,
    pub reserved: Vec<String>,
}
impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_kb: 32,
            max_count: 10,
            reserved: vec![],
        }
    }
}
impl ImportLimits {
    pub fn new(max_count: usize, max_kb: usize) -> Self {
        Self {
            max_kb,
            max_count,
            reserved: vec![],
        }
    }
    fn kb(&self) -> usize {
        self.max_kb.clamp(1, MAX_CHAR_KB)
    }
    fn count(&self) -> usize {
        self.max_count.min(MAX_CHARS_PER_SERVER)
    }
}

/// One bot's server data folder.
#[derive(Clone, Debug)]
pub struct ServerStore {
    dir: PathBuf,
}
impl ServerStore {
    /// `data_dir()/discord/<bot>`, the folder the bot's memory files use.
    pub fn for_bot(bot: &str) -> Self {
        Self::at(data_dir().join("discord").join(sanitize(bot)))
    }
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    fn auto_path(&self) -> PathBuf {
        self.dir.join(AUTO_FILE)
    }
    fn chars_path(&self, guild: u64) -> PathBuf {
        self.dir
            .join("servers")
            .join(guild.to_string())
            .join(CHARS_FILE)
    }

    pub fn auto_channels(&self) -> AutoChannels {
        let mut auto: AutoChannels = read_capped(&self.auto_path(), AUTO_FILE_CAP);
        auto.normalize();
        auto
    }
    pub fn save_auto_channels(&self, auto: &AutoChannels) -> Result<()> {
        let _guard = io_lock();
        self.write_auto(auto)
    }
    fn write_auto(&self, auto: &AutoChannels) -> Result<()> {
        let mut auto = auto.clone();
        auto.normalize();
        config::save_json(&self.auto_path(), &auto)
    }
    /// Turns one auto channel on or off and saves; returns whether anything changed.
    pub fn set_auto_channel(&self, guild: u64, channel: u64, on: bool) -> Result<bool> {
        let _guard = io_lock();
        let mut auto = self.auto_channels();
        let changed = auto.set(guild, channel, on)?;
        if changed {
            self.write_auto(&auto)?;
        }
        Ok(changed)
    }
    /// Turns off every auto channel of one server.
    #[allow(dead_code)]
    pub fn remove_auto_guild(&self, guild: u64) -> Result<bool> {
        let _guard = io_lock();
        let mut auto = self.auto_channels();
        let changed = auto.remove_guild(guild);
        if changed {
            self.write_auto(&auto)?;
        }
        Ok(changed)
    }

    /// The server's characters; entries that break a limit are left out.
    pub fn characters(&self, guild: u64) -> Vec<Persona> {
        if guild == 0 {
            return vec![];
        }
        let list: Vec<Persona> = read_capped(&self.chars_path(guild), CHARS_FILE_CAP);
        let mut names = HashSet::new();
        list.into_iter()
            .map(server_owned)
            .filter(|p| check_character(p).is_ok() && names.insert(name_key(&p.name)))
            .take(MAX_CHARS_PER_SERVER)
            .collect()
    }
    pub fn save_characters(&self, guild: u64, list: &[Persona]) -> Result<()> {
        let _guard = io_lock();
        self.write_characters(guild, list)
    }
    fn write_characters(&self, guild: u64, list: &[Persona]) -> Result<()> {
        ensure!(guild > 0, "Server characters need a server");
        ensure!(
            list.len() <= MAX_CHARS_PER_SERVER,
            "At most {MAX_CHARS_PER_SERVER} characters per server"
        );
        let mut names = HashSet::new();
        let list: Vec<Persona> = list.iter().cloned().map(server_owned).collect();
        for p in &list {
            check_character(p)?;
            ensure!(
                names.insert(name_key(&p.name)),
                "Character names must be unique ({})",
                p.name
            );
        }
        let bytes = serde_json::to_vec_pretty(&list)?;
        ensure!(
            bytes.len() as u64 <= CHARS_FILE_CAP,
            "The server's characters are too large to save"
        );
        config::write_atomic(&self.chars_path(guild), &bytes)
    }
    /// Imports a character file into a server and saves it.
    pub fn import_character(
        &self,
        guild: u64,
        bytes: &[u8],
        limits: &ImportLimits,
    ) -> Result<Persona> {
        ensure!(guild > 0, "Server characters need a server");
        let _guard = io_lock();
        let mut list = self.characters(guild);
        let persona = import_character(bytes, &list, limits)?;
        list.push(persona.clone());
        self.write_characters(guild, &list)?;
        Ok(persona)
    }
    /// Removes a character by name (case-insensitive); returns it when found.
    pub fn remove_character(&self, guild: u64, name: &str) -> Result<Option<Persona>> {
        let _guard = io_lock();
        let mut list = self.characters(guild);
        let key = name_key(name);
        let Some(i) = list.iter().position(|p| name_key(&p.name) == key) else {
            return Ok(None);
        };
        let removed = list.remove(i);
        self.write_characters(guild, &list)?;
        Ok(Some(removed))
    }
    /// A server character as a Tavern V2 card: (file name, JSON bytes).
    pub fn export_character(&self, guild: u64, name: &str) -> Result<(String, Vec<u8>)> {
        let list = self.characters(guild);
        let p = find_character(&list, name)
            .ok_or_else(|| anyhow!("No server character named {:?}", name.trim()))?;
        Ok((export_file_name(&p.name), export_character(p)?))
    }
    /// Servers that have a characters file, ascending.
    pub fn character_guilds(&self) -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir(self.dir.join("servers")) else {
            return vec![];
        };
        let mut guilds: Vec<u64> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
            .filter(|&g| g > 0 && self.chars_path(g).is_file())
            .collect();
        guilds.sort_unstable();
        guilds
    }
}

pub fn load_auto_channels(bot: &str) -> AutoChannels {
    ServerStore::for_bot(bot).auto_channels()
}
#[allow(dead_code)]
pub fn save_auto_channels(bot: &str, auto: &AutoChannels) -> Result<()> {
    ServerStore::for_bot(bot).save_auto_channels(auto)
}
pub fn load_characters(bot: &str, guild: u64) -> Vec<Persona> {
    ServerStore::for_bot(bot).characters(guild)
}
pub fn save_characters(bot: &str, guild: u64, list: &[Persona]) -> Result<()> {
    ServerStore::for_bot(bot).save_characters(guild, list)
}

/// A character by name, case-insensitive, ignoring a leading '@'.
pub fn find_character<'a>(list: &'a [Persona], name: &str) -> Option<&'a Persona> {
    let key = name_key(name);
    list.iter().find(|p| name_key(&p.name) == key)
}

/// Reads JSON (or its `.bak`) unless it is larger than `cap`; anything unreadable is the default.
fn read_capped<T: DeserializeOwned + Default>(path: &Path, cap: u64) -> T {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".bak");
    let too_big = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > cap);
    if too_big(path) || too_big(Path::new(&backup)) {
        return T::default();
    }
    config::read_json(path).0.unwrap_or_default()
}

fn name_key(name: &str) -> String {
    name.trim().trim_start_matches('@').trim().to_lowercase()
}

/// Server characters never choose a model and always answer on Discord, in groups.
fn server_owned(mut p: Persona) -> Persona {
    p.model.clear();
    p.discord = true;
    p.in_group = true;
    p.memory.clear();
    p.turn_order = 0;
    p
}

fn check_character(p: &Persona) -> Result<()> {
    let n = p.name.trim().chars().count();
    ensure!(
        (1..=MAX_NAME).contains(&n),
        "Character names must be 1–{MAX_NAME} characters"
    );
    ensure!(
        p.instructions.chars().count() <= MAX_INSTRUCTIONS,
        "{}: instructions exceed {MAX_INSTRUCTIONS} characters",
        p.name
    );
    ensure!(
        p.lorebook.len() <= MAX_LORE_ENTRIES
            && p.lorebook
                .iter()
                .all(|e| e.text.chars().count() <= MAX_LORE_TEXT),
        "{}: at most {MAX_LORE_ENTRIES} lore entries of {MAX_LORE_TEXT} characters",
        p.name
    );
    personas::validate(std::slice::from_ref(p))
}

/// Parses a character file: this app's character JSON (one object, or an array whose first
/// item is used) or a Tavern / SillyTavern card (V2 `chara_card_v2`, V3 data, or V1 flat
/// fields). Instructions and lore are trimmed to the limits; the name must be 1–64
/// characters and unique in `existing` and `limits.reserved` (case-insensitive), and
/// `existing` must be below the server's character limit.
pub fn import_character(
    bytes: &[u8],
    existing: &[Persona],
    limits: &ImportLimits,
) -> Result<Persona> {
    ensure!(
        bytes.len() <= limits.kb() * 1024,
        "The file is {} KB; the limit is {} KB",
        bytes.len().div_ceil(1024),
        limits.kb()
    );
    ensure!(
        existing.len() < limits.count(),
        "This server has {} of {} characters already",
        existing.len(),
        limits.count()
    );
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| anyhow!("Not a character JSON file ({e})"))?;
    let value = match value {
        Value::Array(items) => items
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("The file has no characters"))?,
        value => value,
    };
    let Value::Object(obj) = value else {
        bail!("Expected a character object");
    };
    let raw = parse_card(&obj)?;
    finish(raw, existing, limits)
}

/// The card's fields before limits are applied.
struct RawCard {
    name: String,
    instructions: String,
    avatar: String,
    lore: Vec<LoreEntry>,
}

fn parse_card(obj: &Map<String, Value>) -> Result<RawCard> {
    let spec = text(obj, "spec");
    if let Some(data) = obj.get("data").and_then(Value::as_object)
        && (spec.starts_with("chara_card_v") || data.contains_key("name"))
    {
        return Ok(from_tavern(data, obj));
    }
    if obj.contains_key("instructions") {
        return from_app(obj);
    }
    if obj.contains_key("name") || obj.contains_key("char_name") {
        return Ok(from_tavern(obj, obj));
    }
    bail!("Unknown character format: use this app's character JSON or a Tavern V1/V2 card")
}

fn from_app(obj: &Map<String, Value>) -> Result<RawCard> {
    ensure!(
        obj.get("name").is_some_and(Value::is_string),
        "The character has no name"
    );
    let p: Persona = serde_json::from_value(Value::Object(obj.clone()))
        .map_err(|e| anyhow!("Not a valid character ({e})"))?;
    Ok(RawCard {
        name: p.name,
        instructions: p.instructions,
        avatar: p.avatar_url,
        lore: p.lorebook,
    })
}

fn from_tavern(data: &Map<String, Value>, outer: &Map<String, Value>) -> RawCard {
    let name = first(data, &["name", "char_name"]);
    let sections = [
        (
            "",
            first(data, &["system_prompt"]).replace("{{original}}", ""),
        ),
        ("", first(data, &["description", "char_persona"])),
        ("Personality: ", first(data, &["personality"])),
        ("Scenario: ", first(data, &["scenario", "world_scenario"])),
        (
            "Example dialogue:\n",
            first(data, &["mes_example", "example_dialogue"]),
        ),
    ];
    let joined = sections
        .iter()
        .filter(|(_, body)| !body.trim().is_empty())
        .map(|(label, body)| format!("{label}{}", body.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut avatar = text(data, "avatar");
    if avatar.is_empty() {
        avatar = text(outer, "avatar");
    }
    let lore = data
        .get("character_book")
        .or_else(|| outer.get("character_book"))
        .map(book_entries)
        .unwrap_or_default();
    RawCard {
        instructions: macros(&joined, name.trim()),
        lore: lore
            .into_iter()
            .map(|e| LoreEntry {
                keys: e.keys.iter().map(|k| macros(k, name.trim())).collect(),
                text: macros(&e.text, name.trim()),
            })
            .collect(),
        name,
        avatar,
    }
}

/// Lore from a Tavern `character_book` (V2 entry list) or a SillyTavern world-info
/// object (`entries` keyed by number, `key` arrays). Disabled entries are skipped.
fn book_entries(book: &Value) -> Vec<LoreEntry> {
    let entries: Vec<&Value> = match book.get("entries") {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => return vec![],
    };
    entries
        .into_iter()
        .filter_map(Value::as_object)
        .filter(|e| {
            e.get("enabled").and_then(Value::as_bool) != Some(false)
                && e.get("disable").and_then(Value::as_bool) != Some(true)
        })
        .map(|e| LoreEntry {
            keys: e
                .get("keys")
                .or_else(|| e.get("key"))
                .and_then(Value::as_array)
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            text: text(e, "content"),
        })
        .collect()
}

/// Replaces Tavern placeholders: the character's name and the person talking to it.
fn macros(s: &str, name: &str) -> String {
    let mut out = s.to_owned();
    for tag in ["{{char}}", "{{Char}}", "{{CHAR}}", "<BOT>", "<bot>"] {
        out = out.replace(tag, name);
    }
    for tag in ["{{user}}", "{{User}}", "{{USER}}", "<USER>", "<user>"] {
        out = out.replace(tag, "the user");
    }
    out
}

fn text(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn first(obj: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| text(obj, k))
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default()
}

/// The first `max` characters of `s`, without trailing whitespace.
fn cap(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A display name on one line: control characters dropped, spaces collapsed, no leading '@'.
fn clean_name(name: &str) -> Result<String> {
    let flat: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = flat
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_start_matches('@')
        .trim()
        .to_owned();
    ensure!(
        (1..=MAX_NAME).contains(&name.chars().count()),
        "Character names must be 1–{MAX_NAME} characters"
    );
    ensure!(
        !matches!(name.to_lowercase().as_str(), "everyone" | "here"),
        "{name:?} cannot be a character name"
    );
    Ok(name)
}

fn https_avatar(url: &str) -> String {
    let url = url.trim();
    if url.starts_with("https://")
        && url.len() > "https://".len()
        && url.len() <= MAX_AVATAR
        && !url.contains(|c: char| c.is_whitespace() || c.is_control())
    {
        url.to_owned()
    } else {
        String::new()
    }
}

fn finish(raw: RawCard, existing: &[Persona], limits: &ImportLimits) -> Result<Persona> {
    let name = clean_name(&raw.name)?;
    let key = name_key(&name);
    ensure!(
        !existing.iter().any(|p| name_key(&p.name) == key),
        "This server already has a character named {name}"
    );
    ensure!(
        !limits.reserved.iter().any(|r| name_key(r) == key),
        "The name {name} is already used by this bot"
    );
    let lorebook = raw
        .lore
        .into_iter()
        .filter_map(|e| {
            let keys: Vec<String> = e
                .keys
                .iter()
                .map(|k| cap(k.trim(), MAX_KEY))
                .filter(|k| !k.is_empty())
                .take(MAX_LORE_KEYS)
                .collect();
            let text = cap(e.text.trim(), MAX_LORE_TEXT);
            (!keys.is_empty() && !text.is_empty()).then_some(LoreEntry { keys, text })
        })
        .take(MAX_LORE_ENTRIES)
        .collect();
    let persona = Persona {
        name,
        instructions: cap(raw.instructions.trim(), MAX_INSTRUCTIONS),
        avatar_url: https_avatar(&raw.avatar),
        model: String::new(),
        in_group: true,
        discord: true,
        lorebook,
        memory: vec![],
        turn_order: 0,
    };
    check_character(&persona)?;
    Ok(persona)
}

/// A character as a Tavern V2 card (SillyTavern imports it; so does [`import_character`]).
pub fn export_character(p: &Persona) -> Result<Vec<u8>> {
    let mut data = json!({
        "name": p.name,
        "description": p.instructions,
        "personality": "",
        "scenario": "",
        "first_mes": "",
        "mes_example": "",
        "creator_notes": "Exported from KYTTU - AI",
        "system_prompt": "",
        "post_history_instructions": "",
        "alternate_greetings": [],
        "tags": [],
        "creator": "",
        "character_version": "",
        "extensions": {},
    });
    if !p.lorebook.is_empty() {
        let entries: Vec<Value> = p
            .lorebook
            .iter()
            .enumerate()
            .map(|(i, e)| {
                json!({
                    "keys": e.keys,
                    "content": e.text,
                    "extensions": {},
                    "enabled": true,
                    "insertion_order": i,
                    "case_sensitive": false,
                    "id": i,
                })
            })
            .collect();
        data["character_book"] = json!({ "entries": entries, "extensions": {} });
    }
    let mut card = json!({ "spec": "chara_card_v2", "spec_version": "2.0", "data": data });
    if !p.avatar_url.is_empty() {
        // V1 field; V2 readers ignore it, this app reads it back.
        card["avatar"] = json!(p.avatar_url);
    }
    Ok(serde_json::to_vec_pretty(&card)?)
}

/// Attachment name for an exported character.
pub fn export_file_name(name: &str) -> String {
    format!("{}.json", sanitize(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V2: &str = r#"{
        "spec": "chara_card_v2",
        "spec_version": "2.0",
        "name": "Ignored Outer",
        "data": {
            "name": "Seraphina",
            "description": "{{char}} is a guardian of the forest who helps {{user}}.",
            "personality": "kind, protective",
            "scenario": "<USER> wakes up in a glade.",
            "first_mes": "You're awake!",
            "mes_example": "<START>\n{{user}}: Hi\n{{char}}: Hello, traveler.",
            "system_prompt": "{{original}} Stay in character.",
            "creator_notes": "",
            "tags": ["fantasy"],
            "character_book": {
                "entries": [
                    {"keys": ["Glade", " "], "content": "The glade is warded.", "enabled": true},
                    {"keys": ["secret"], "content": "Hidden.", "enabled": false},
                    {"keys": [], "content": "No keys."},
                    {"keys": ["{{char}}"], "content": "{{char}} is old."}
                ]
            }
        }
    }"#;
    const V1: &str = r#"{
        "name": "Aqua",
        "description": "A cheerful water goddess.",
        "personality": "loud",
        "scenario": "",
        "first_mes": "Hi!",
        "mes_example": "",
        "avatar": "https://example.com/aqua.png"
    }"#;

    fn limits() -> ImportLimits {
        ImportLimits::default()
    }
    fn named(name: &str) -> Persona {
        Persona {
            name: name.into(),
            instructions: "x".into(),
            ..Default::default()
        }
    }

    #[test]
    fn imports_v2_card() {
        let p = import_character(V2.as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Seraphina");
        assert_eq!(
            p.instructions,
            "Stay in character.\n\n\
             Seraphina is a guardian of the forest who helps the user.\n\n\
             Personality: kind, protective\n\n\
             Scenario: the user wakes up in a glade.\n\n\
             Example dialogue:\n<START>\nthe user: Hi\nSeraphina: Hello, traveler."
        );
        assert_eq!(p.lorebook.len(), 2);
        assert_eq!(p.lorebook[0].keys, ["Glade"]);
        assert_eq!(p.lorebook[0].text, "The glade is warded.");
        assert_eq!(p.lorebook[1].keys, ["Seraphina"]);
        assert_eq!(p.lorebook[1].text, "Seraphina is old.");
        assert!(p.discord && p.in_group && p.model.is_empty() && p.avatar_url.is_empty());
    }

    #[test]
    fn imports_v1_card_and_sillytavern_world_info() {
        let p = import_character(V1.as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Aqua");
        assert_eq!(
            p.instructions,
            "A cheerful water goddess.\n\nPersonality: loud"
        );
        assert_eq!(p.avatar_url, "https://example.com/aqua.png");
        let old = r#"{"char_name":"Bob","char_persona":"A builder.","world_scenario":"A site.",
            "character_book":{"entries":{"0":{"key":["hammer"],"content":"Bob's hammer."},
            "1":{"key":["x"],"content":"off","disable":true}}}}"#;
        let p = import_character(old.as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Bob");
        assert_eq!(p.instructions, "A builder.\n\nScenario: A site.");
        assert_eq!(p.lorebook.len(), 1);
        assert_eq!(p.lorebook[0].keys, ["hammer"]);
    }

    #[test]
    fn only_https_avatars_are_kept() {
        for (url, kept) in [
            ("https://cdn.example.com/a.png", true),
            ("http://cdn.example.com/a.png", false),
            ("none", false),
            ("javascript:alert(1)", false),
            ("https://", false),
            ("https://a b.png", false),
        ] {
            let card = json!({"name": "A", "description": "d", "avatar": url}).to_string();
            let p = import_character(card.as_bytes(), &[], &limits()).unwrap();
            assert_eq!(p.avatar_url.is_empty(), !kept, "{url}");
        }
        let long = format!("https://e.com/{}", "a".repeat(MAX_AVATAR));
        let card = json!({"name": "A", "avatar": long}).to_string();
        assert!(
            import_character(card.as_bytes(), &[], &limits())
                .unwrap()
                .avatar_url
                .is_empty()
        );
    }

    #[test]
    fn imports_app_json_object_and_array() {
        let app = json!({
            "name": "  Coder  ",
            "instructions": "Write Rust.",
            "avatar_url": "https://e.com/c.png",
            "model": "secret-model",
            "in_group": false,
            "discord": false,
            "lorebook": [{"keys": ["borrow"], "text": "Borrow checker notes."}],
            "memory": ["remember me"],
            "turn_order": 7
        });
        let p = import_character(app.to_string().as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Coder");
        assert_eq!(p.instructions, "Write Rust.");
        assert_eq!(p.avatar_url, "https://e.com/c.png");
        assert!(p.model.is_empty() && p.discord && p.in_group);
        assert!(p.memory.is_empty() && p.turn_order == 0);
        assert_eq!(p.lorebook[0].text, "Borrow checker notes.");
        let list = json!([app, {"name": "Second", "instructions": ""}]).to_string();
        let p = import_character(list.as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Coder");
        // A UTF-8 BOM is accepted.
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend_from_slice(V1.as_bytes());
        assert_eq!(import_character(&bom, &[], &limits()).unwrap().name, "Aqua");
    }

    #[test]
    fn rejects_bad_files() {
        for bad in [
            "",
            "not json",
            "[]",
            "42",
            r#""text""#,
            r#"{"foo": 1}"#,
            r#"{"instructions": "no name"}"#,
            r#"{"name": 5, "instructions": "x"}"#,
        ] {
            assert!(
                import_character(bad.as_bytes(), &[], &limits()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn enforces_size_limit() {
        let big = json!({"name": "Big", "description": "x".repeat(40 * 1024)}).to_string();
        let err = import_character(big.as_bytes(), &[], &limits()).unwrap_err();
        assert!(err.to_string().contains("limit is 32 KB"), "{err}");
        let roomy = ImportLimits::new(10, 64);
        let p = import_character(big.as_bytes(), &[], &roomy).unwrap();
        assert_eq!(p.instructions.chars().count(), MAX_INSTRUCTIONS);
        // The setting cannot raise the ceiling.
        let huge = ImportLimits::new(10, 100_000);
        let blob = vec![b' '; (MAX_CHAR_KB + 1) * 1024];
        assert!(import_character(&blob, &[], &huge).is_err());
    }

    #[test]
    fn name_rules() {
        let card = |name: &str| json!({"name": name, "description": "d"}).to_string();
        assert!(import_character(card("").as_bytes(), &[], &limits()).is_err());
        assert!(import_character(card("   ").as_bytes(), &[], &limits()).is_err());
        assert!(import_character(card(&"n".repeat(65)).as_bytes(), &[], &limits()).is_err());
        assert!(import_character(card("everyone").as_bytes(), &[], &limits()).is_err());
        let p = import_character(card(&"n".repeat(64)).as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name.chars().count(), 64);
        let p = import_character(card("@Dr.\n  Who\u{0}").as_bytes(), &[], &limits()).unwrap();
        assert_eq!(p.name, "Dr. Who");
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let existing = [named("Aqua")];
        let card = json!({"name": "  aQUA ", "description": "d"}).to_string();
        let err = import_character(card.as_bytes(), &existing, &limits()).unwrap_err();
        assert!(err.to_string().contains("already has"), "{err}");
        let mut reserved = limits();
        reserved.reserved = vec!["Darkness".into()];
        let card = json!({"name": "darkness", "description": "d"}).to_string();
        assert!(import_character(card.as_bytes(), &[], &reserved).is_err());
        let card = json!({"name": "Megumin", "description": "d"}).to_string();
        assert!(import_character(card.as_bytes(), &existing, &reserved).is_ok());
    }

    #[test]
    fn count_limit() {
        let three: Vec<Persona> = (0..3).map(|i| named(&format!("C{i}"))).collect();
        let lim = ImportLimits::new(3, 32);
        let err = import_character(V1.as_bytes(), &three, &lim).unwrap_err();
        assert!(err.to_string().contains("3 of 3"), "{err}");
        assert!(import_character(V1.as_bytes(), &three[..2], &lim).is_ok());
        assert!(import_character(V1.as_bytes(), &[], &ImportLimits::new(0, 32)).is_err());
        let many: Vec<Persona> = (0..MAX_CHARS_PER_SERVER)
            .map(|i| named(&format!("C{i}")))
            .collect();
        assert!(import_character(V1.as_bytes(), &many, &ImportLimits::new(1000, 32)).is_err());
    }

    #[test]
    fn lore_and_instructions_are_trimmed_to_limits() {
        // 60 entries of 25 long keys: above every limit, below the 256 KB file cap.
        let entries: Vec<Value> = (0..60)
            .map(|i| {
                let keys: Vec<String> = (0..25)
                    .map(|k| format!("k{i}-{k}{}", "x".repeat(70)))
                    .collect();
                json!({ "keys": keys, "content": "y".repeat(1100) })
            })
            .collect();
        let card = json!({
            "spec": "chara_card_v2",
            "data": {
                "name": "Lore",
                "description": "d".repeat(5000),
                "character_book": {"entries": entries},
            }
        })
        .to_string();
        let p = import_character(card.as_bytes(), &[], &ImportLimits::new(10, 256)).unwrap();
        assert_eq!(p.instructions.chars().count(), MAX_INSTRUCTIONS);
        assert_eq!(p.lorebook.len(), MAX_LORE_ENTRIES);
        for e in &p.lorebook {
            assert_eq!(e.text.chars().count(), MAX_LORE_TEXT);
            assert_eq!(e.keys.len(), MAX_LORE_KEYS);
            assert!(e.keys.iter().all(|k| k.chars().count() <= MAX_KEY));
        }
        assert!(personas::validate(std::slice::from_ref(&p)).is_ok());
        // Multi-byte text is cut on character boundaries.
        let card = json!({"name": "É", "description": "é".repeat(5000)}).to_string();
        let p = import_character(card.as_bytes(), &[], &ImportLimits::new(10, 64)).unwrap();
        assert_eq!(p.instructions.chars().count(), MAX_INSTRUCTIONS);
    }

    #[test]
    fn export_round_trips() {
        let p = import_character(V2.as_bytes(), &[], &limits()).unwrap();
        let mut with_avatar = p.clone();
        with_avatar.avatar_url = "https://e.com/s.png".into();
        let bytes = export_character(&with_avatar).unwrap();
        let card: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(card["spec"], "chara_card_v2");
        assert_eq!(card["data"]["name"], "Seraphina");
        let back = import_character(&bytes, &[], &limits()).unwrap();
        assert_eq!(back.name, p.name);
        assert_eq!(back.instructions, p.instructions);
        assert_eq!(back.lorebook, p.lorebook);
        assert_eq!(back.avatar_url, "https://e.com/s.png");
        assert_eq!(export_file_name("Dr. Who"), "dr__who.json");
    }

    #[test]
    fn auto_channels_rules() {
        let mut auto = AutoChannels::default();
        assert!(auto.set(1, 10, true).unwrap());
        assert!(!auto.set(1, 10, true).unwrap());
        assert!(auto.contains(1, 10) && !auto.contains(2, 10));
        assert!(auto.set(0, 10, true).is_err() && auto.set(1, 0, true).is_err());
        for c in 11..(10 + MAX_AUTO_PER_GUILD as u64) {
            auto.set(1, c, true).unwrap();
        }
        assert!(auto.set(1, 999, true).is_err());
        assert!(auto.set(1, 10, false).unwrap());
        assert!(!auto.set(1, 10, false).unwrap());
        assert!(!auto.set(5, 10, false).unwrap());
        assert_eq!(auto.channels(1).len(), MAX_AUTO_PER_GUILD - 1);
        assert!(auto.remove_guild(1) && auto.guilds().is_empty());
        for g in 1..=MAX_AUTO_GUILDS as u64 {
            auto.set(g, 1, true).unwrap();
        }
        assert!(auto.set(MAX_AUTO_GUILDS as u64 + 1, 1, true).is_err());
        assert!(auto.set(1, 2, true).unwrap());
    }

    #[test]
    fn auto_channels_persist_with_string_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServerStore::at(dir.path());
        assert!(store.auto_channels().0.is_empty());
        assert!(store.set_auto_channel(123, 456, true).unwrap());
        assert!(store.set_auto_channel(123, 789, true).unwrap());
        assert!(store.auto_channels().contains(123, 456));
        let raw = std::fs::read_to_string(dir.path().join(AUTO_FILE)).unwrap();
        assert!(raw.contains("\"123\""), "{raw}");
        assert!(store.set_auto_channel(123, 456, false).unwrap());
        assert_eq!(store.auto_channels().channels(123), [789]);
        assert!(store.remove_auto_guild(123).unwrap());
        assert!(store.auto_channels().0.is_empty());
        // Zero IDs and empty servers in a hand-edited file are dropped.
        std::fs::write(dir.path().join(AUTO_FILE), r#"{"0":[1],"5":[],"6":[0,7]}"#).unwrap();
        assert_eq!(store.auto_channels().guilds(), [6]);
        assert_eq!(store.auto_channels().channels(6), [7]);
    }

    #[test]
    fn oversized_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServerStore::at(dir.path());
        let mut big = String::from("{\"1\":[");
        while big.len() as u64 <= AUTO_FILE_CAP {
            big.push_str("1234567890,");
        }
        big.push_str("1]}");
        std::fs::write(dir.path().join(AUTO_FILE), &big).unwrap();
        assert!(store.auto_channels().0.is_empty());
        let path = store.chars_path(9);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(CHARS_FILE_CAP + 1).unwrap();
        assert!(store.characters(9).is_empty());
    }

    #[test]
    fn server_characters_import_list_remove_export() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServerStore::at(dir.path());
        assert!(store.characters(42).is_empty() && store.character_guilds().is_empty());
        let lim = ImportLimits::new(2, 32);
        store.import_character(42, V1.as_bytes(), &lim).unwrap();
        store.import_character(42, V2.as_bytes(), &lim).unwrap();
        assert!(dir.path().join("servers/42/characters.json").is_file());
        assert!(store.import_character(42, V1.as_bytes(), &lim).is_err());
        assert!(store.import_character(0, V1.as_bytes(), &lim).is_err());
        let names: Vec<String> = store.characters(42).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Aqua", "Seraphina"]);
        assert_eq!(store.character_guilds(), [42]);
        // Other servers are separate.
        store.import_character(7, V1.as_bytes(), &lim).unwrap();
        assert_eq!(store.character_guilds(), [7, 42]);
        let (file, bytes) = store.export_character(42, "@seraphina").unwrap();
        assert_eq!(file, "seraphina.json");
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
        assert!(store.export_character(42, "Nobody").is_err());
        assert_eq!(
            store.remove_character(42, "AQUA").unwrap().unwrap().name,
            "Aqua"
        );
        assert!(store.remove_character(42, "Aqua").unwrap().is_none());
        assert_eq!(store.characters(42).len(), 1);
        assert!(find_character(&store.characters(42), " SERAPHINA ").is_some());
    }

    #[test]
    fn loaded_characters_are_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        let store = ServerStore::at(dir.path());
        let mut model = named("Model");
        model.model = "big-model".into();
        model.discord = false;
        let bad_avatar = Persona {
            avatar_url: "http://e.com/a.png".into(),
            ..named("Http")
        };
        let long = Persona {
            instructions: "x".repeat(MAX_INSTRUCTIONS + 1),
            ..named("Long")
        };
        let list = [model, named("model"), bad_avatar, long, named("")];
        let path = store.chars_path(3);
        config::save_json(&path, &list).unwrap();
        let loaded = store.characters(3);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "Model");
        assert!(loaded[0].model.is_empty() && loaded[0].discord);
        // Saving refuses the same list.
        assert!(store.save_characters(3, &list).is_err());
        assert!(store.save_characters(3, &[named("A"), named("a")]).is_err());
        assert!(store.save_characters(0, &[]).is_err());
        store.save_characters(3, &loaded).unwrap();
        assert_eq!(store.characters(3).len(), 1);
    }
}

use crate::{
    config::{Config, data_dir},
    core::personas::Persona,
    docs::Library,
    engine::{self, Event, Request},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::{
    all::{
        ChannelId, Context, CreateAllowedMentions, CreateMessage, EventHandler, ExecuteWebhook,
        GatewayIntents, Message, Ready, Webhook,
    },
    async_trait,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

/// Turns kept per memory file, and the file size cap.
const MAX_TURNS: usize = 40;
const MAX_BYTES: usize = 64 * 1024;
/// Replies one bot may run at once (they still queue for the single local model).
const MAX_ACTIVE: usize = 3;
/// Memory files kept loaded per bot; older ones are evicted (they stay on disk).
const CACHE_SIZE: usize = 64;

/// Which conversation a bot remembers: per channel, per server or per user.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum MemoryScope {
    #[default]
    Channel,
    Server,
    User,
}

/// One Discord bot's settings (its token is kept in memory only).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BotProfile {
    pub name: String,
    pub channels: Vec<String>,
    pub guilds: Vec<String>,
    pub prefix: String,
    pub group: bool,
    pub docs: bool,
    pub memory: MemoryScope,
    pub cooldown_secs: u64,
    pub reply_timeout_secs: u64,
}
impl Default for BotProfile {
    fn default() -> Self {
        Self {
            name: "Bot 1".into(),
            channels: vec![],
            guilds: vec![],
            prefix: "!ai".into(),
            group: false,
            docs: false,
            memory: MemoryScope::Channel,
            cooldown_secs: 5,
            reply_timeout_secs: 180,
        }
    }
}

/// One bot session. Several can run at once; they share no global state.
pub struct Discord {
    pub status: Arc<Mutex<String>>,
    pub running: Arc<AtomicBool>,
    stop: Mutex<Arc<AtomicBool>>,
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
}
impl Default for Discord {
    fn default() -> Self {
        Self {
            status: Arc::new(Mutex::new("Disconnected".into())),
            running: Arc::new(AtomicBool::new(false)),
            stop: Mutex::new(Arc::new(AtomicBool::new(false))),
            replies: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
impl Discord {
    pub fn disconnect(&self) {
        self.stop.lock().unwrap().store(true, Ordering::Relaxed);
        self.stop_replies();
    }
    /// Cancels every reply this bot is generating, without disconnecting.
    pub fn stop_replies(&self) {
        for flag in self.replies.lock().unwrap().values() {
            flag.store(true, Ordering::Relaxed);
        }
    }
    pub fn connect(
        &self,
        token: String,
        webhooks: HashMap<String, String>,
        profile: BotProfile,
        config: Config,
        library: Library,
    ) -> Result<()> {
        ensure!(
            !self.running.load(Ordering::Relaxed),
            "Disconnect this bot first"
        );
        let token = token.trim().to_owned();
        ensure!(!profile.name.trim().is_empty(), "Name the bot");
        ensure!(
            !token.is_empty(),
            "Enter the Discord bot token for {}",
            profile.name.trim()
        );
        let mut profile = profile;
        profile.prefix = profile.prefix.trim().to_owned();
        ensure!(
            !profile.prefix.is_empty() && !profile.prefix.contains(char::is_whitespace),
            "A command prefix without spaces is required"
        );
        let channels = parse_ids(&profile.channels).map_err(|e| anyhow!("Channels: {e}"))?;
        ensure!(!channels.is_empty(), "List at least one channel ID");
        let guilds = parse_ids(&profile.guilds).map_err(|e| anyhow!("Servers: {e}"))?;
        let mut hooks = HashMap::new();
        for (channel, url) in webhooks {
            let url = url.trim();
            if url.is_empty() {
                continue;
            }
            let id = channel
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|&id| id > 0)
                .ok_or_else(|| anyhow!("Webhook channel {channel:?} is not a numeric ID"))?;
            ensure!(
                channels.contains(&id),
                "Webhook channel {id} is not in this bot's channel list"
            );
            validate_webhook(url)?;
            hooks.insert(id, url.to_owned());
        }
        if self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            bail!("Disconnect this bot first");
        }
        let stop = Arc::new(AtomicBool::new(false));
        *self.stop.lock().unwrap() = stop.clone();
        self.replies.lock().unwrap().clear();
        let running = self.running.clone();
        let status = self.status.clone();
        *status.lock().unwrap() = "Connecting…".into();
        let handler = Handler {
            memory: Memory::new(data_dir().join("discord").join(sanitize(&profile.name))),
            config,
            library,
            profile,
            channels,
            guilds,
            webhooks: hooks,
            status: status.clone(),
            stop: stop.clone(),
            replies: self.replies.clone(),
            cooldowns: Mutex::new(HashMap::new()),
        };
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            if let Ok(runtime) = result {
                runtime.block_on(run_client(token, handler, status.clone(), stop));
            } else {
                *status.lock().unwrap() = "Could not initialize Discord runtime".into();
            }
            running.store(false, Ordering::Relaxed);
        });
        Ok(())
    }
}

async fn run_client(
    token: String,
    handler: Handler,
    status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
) {
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = match serenity::Client::builder(&token, intents)
        .event_handler(handler)
        .await
    {
        Ok(client) => client,
        Err(_) => {
            *status.lock().unwrap() = "Could not connect. Check your bot token and network.".into();
            return;
        }
    };
    if stop.load(Ordering::Relaxed) {
        *status.lock().unwrap() = "Disconnected".into();
        return;
    }
    let manager = client.shard_manager.clone();
    let stopping = stop.clone();
    let watcher = tokio::spawn(async move {
        while !stopping.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        manager.shutdown_all().await;
    });
    *status.lock().unwrap() = if client.start().await.is_err() {
        "Connection failed. Check token, Message Content Intent, channel access, and network."
            .into()
    } else {
        "Disconnected".into()
    };
    stop.store(true, Ordering::Relaxed);
    watcher.abort();
}

struct Handler {
    config: Config,
    library: Library,
    profile: BotProfile,
    channels: Vec<u64>,
    guilds: Vec<u64>,
    webhooks: HashMap<u64, String>,
    status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    cooldowns: Mutex<HashMap<u64, Instant>>,
    memory: Memory,
}
/// Removes a channel's running reply when the handler returns, however it returns.
struct ActiveReply {
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    channel: u64,
}
impl Drop for ActiveReply {
    fn drop(&mut self) {
        self.replies.lock().unwrap().remove(&self.channel);
    }
}
impl Handler {
    fn set_status(&self, text: impl Into<String>) {
        *self.status.lock().unwrap() = text.into();
    }
    /// True (and records the time) when this user may make a request now.
    fn cooldown_ok(&self, user: u64) -> bool {
        let wait = Duration::from_secs(self.profile.cooldown_secs);
        let now = Instant::now();
        let mut map = self.cooldowns.lock().unwrap();
        if map
            .get(&user)
            .is_some_and(|t| now.duration_since(*t) < wait)
        {
            return false;
        }
        if map.len() >= 1000 {
            map.retain(|_, t| now.duration_since(*t) < wait);
        }
        map.insert(user, now);
        true
    }
}
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        self.set_status(format!(
            "Connected as {} · {} channel(s)",
            ready.user.name,
            self.channels.len()
        ));
    }
    async fn message(&self, ctx: Context, msg: Message) {
        if self.stop.load(Ordering::Relaxed) || msg.author.bot || msg.webhook_id.is_some() {
            return;
        }
        let channel = msg.channel_id.get();
        let guild = msg.guild_id.map(|g| g.get());
        let user = msg.author.id.get();
        if !allowed(&self.channels, &self.guilds, channel, guild) {
            return;
        }
        let Some(command) = parse_command(&msg.content, &self.profile.prefix) else {
            return;
        };
        let key = scope_key(&self.profile.memory, channel, guild, user);
        if !self.cooldown_ok(user) {
            return;
        }
        if command.eq_ignore_ascii_case("stop") {
            let flag = self.replies.lock().unwrap().get(&channel).cloned();
            let text = match flag {
                Some(flag) => {
                    flag.store(true, Ordering::Relaxed);
                    "Stopping the current reply."
                }
                None => "Nothing is running in this channel.",
            };
            notice(&ctx, msg.channel_id, text).await;
            return;
        }
        if command.eq_ignore_ascii_case("reset") {
            self.memory.clear(&key);
            notice(&ctx, msg.channel_id, "Memory cleared.").await;
            return;
        }
        if !self.config.personas.iter().any(|p| p.discord) {
            notice(&ctx, msg.channel_id, "No characters are available on Discord yet. Turn on \"Available on Discord\" for a character in KYTTU - AI.").await;
            return;
        }
        let (selected, question) =
            select_personas(command, &self.config.personas, self.profile.group);
        if question.is_empty() {
            return;
        }
        if selected.is_empty() {
            notice(&ctx, msg.channel_id, "No Discord character has that name.").await;
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let busy = {
            let mut active = self.replies.lock().unwrap();
            let busy = active.contains_key(&channel) || active.len() >= MAX_ACTIVE;
            if !busy {
                active.insert(channel, cancel.clone());
            }
            busy
        };
        if busy {
            notice(
                &ctx,
                msg.channel_id,
                "Busy with another reply. Try again shortly.",
            )
            .await;
            return;
        }
        let _active = ActiveReply {
            replies: self.replies.clone(),
            channel,
        };
        let webhook = match self.webhooks.get(&channel) {
            None => None,
            Some(url) => match Webhook::from_url(&ctx.http, url).await {
                Ok(hook) if hook.channel_id == Some(msg.channel_id) => Some(hook),
                _ => {
                    self.set_status("Webhook validation failed or it belongs to a different channel. Nothing was posted.");
                    return;
                }
            },
        };
        let speaker_line = format!("{}: {question}", speaker(&msg));
        // Read the generation first so a reset during this reply drops its turn.
        let since = self.memory.generation(&key);
        let history = self.memory.load(&key);
        let mut new_turn = vec![];
        for (index, persona) in selected.iter().enumerate() {
            if self.stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                break;
            }
            self.set_status(format!("{} is responding…", persona.name));
            let mut config = self.config.clone();
            if !persona.model.is_empty() {
                config.model = persona.model.clone();
            }
            let request = Request {
                config,
                library: self.library.clone(),
                question: speaker_line.clone(),
                history: [history.as_slice(), new_turn.as_slice()].concat(),
                coding: false,
                use_docs: self.profile.docs,
                pack: String::new(),
                cancel: cancel.clone(),
                persona: persona.clone(),
                use_web: false,
                use_embeddings: false,
            };
            let (bot_stop, limit) = (self.stop.clone(), self.profile.reply_timeout_secs);
            let generated =
                tokio::task::spawn_blocking(move || run_reply(request, bot_stop, limit)).await;
            let Ok((text, result, timed_out)) = generated else {
                self.set_status("Local model worker crashed.");
                break;
            };
            if timed_out {
                let text = format!(
                    "{} hit the {limit}s time limit and was stopped.",
                    persona.name
                );
                notice(&ctx, msg.channel_id, &text).await;
                break;
            }
            if self.stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                break;
            }
            let returned = match result {
                Ok(returned) => returned,
                Err(e) => {
                    self.set_status(format!(
                        "Local model request failed: {}",
                        e.chars().take(200).collect::<String>()
                    ));
                    notice(
                        &ctx,
                        msg.channel_id,
                        "Sorry, the local model could not answer.",
                    )
                    .await;
                    break;
                }
            };
            append_turn(&mut new_turn, returned, &speaker_line, index == 0);
            let name = persona.name.chars().take(80).collect::<String>();
            for chunk in message_chunks(&text) {
                let result = if let Some(hook) = &webhook {
                    let mut builder = ExecuteWebhook::new()
                        .content(chunk)
                        .username(name.clone())
                        .allowed_mentions(no_mentions());
                    if persona.avatar_url.starts_with("https://") {
                        builder = builder.avatar_url(&persona.avatar_url);
                    }
                    hook.execute(&ctx.http, false, builder).await.map(|_| ())
                } else {
                    msg.channel_id
                        .send_message(
                            &ctx.http,
                            CreateMessage::new()
                                .content(format!("**{name}**\n{chunk}"))
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map(|_| ())
                };
                if result.is_err() {
                    self.set_status(
                        "Could not post. Check bot/webhook permissions and rate limits.",
                    );
                    self.memory.append(&key, new_turn, since);
                    return;
                }
            }
        }
        self.memory.append(&key, new_turn, since);
        if !self.stop.load(Ordering::Relaxed) {
            self.set_status("Connected · waiting for a prefixed message");
        }
    }
}

/// Runs one engine request on a blocking thread with its own cancel flag and time limit
/// (0 = no limit). Returns (streamed text, engine result, whether the time limit hit).
fn run_reply(
    request: Request,
    bot_stop: Arc<AtomicBool>,
    limit_secs: u64,
) -> (String, std::result::Result<Vec<Value>, String>, bool) {
    let cancel = request.cancel.clone();
    let (tx, rx) = mpsc::channel();
    engine::spawn(request, tx);
    let deadline = (limit_secs > 0).then(|| Instant::now() + Duration::from_secs(limit_secs));
    let (mut text, mut timed_out) = (String::new(), false);
    loop {
        if bot_stop.load(Ordering::Relaxed) {
            cancel.store(true, Ordering::Relaxed);
        }
        if !cancel.load(Ordering::Relaxed) && deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            cancel.store(true, Ordering::Relaxed);
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Token(t)) => text.push_str(&t),
            Ok(Event::Approval(a)) => {
                let _ = a.reply.send(false);
            }
            Ok(Event::Finished(result)) => return (text, result, timed_out),
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return (text, Err("Model worker disconnected".into()), timed_out);
            }
        }
    }
}

async fn notice(ctx: &Context, channel: ChannelId, text: &str) {
    let message = CreateMessage::new()
        .content(text)
        .allowed_mentions(no_mentions());
    let _ = channel.send_message(&ctx.http, message).await;
}
fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}
/// The author's display name, cleaned for use inside a prompt.
fn speaker(msg: &Message) -> String {
    let name = msg
        .author
        .global_name
        .as_deref()
        .unwrap_or(&msg.author.name);
    let name: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    match name.trim() {
        "" => "User".into(),
        name => name.into(),
    }
}

/// Saved conversation memory for one bot: data_dir()/discord/<bot>/<scope>-<id>.json.
/// Lock order: `cache`, then `generations`.
struct Memory {
    dir: PathBuf,
    cache: Mutex<HashMap<String, (Vec<Value>, Instant)>>,
    /// Bumped by `clear`, so a reply that started before a reset does not restore it.
    generations: Mutex<HashMap<String, u64>>,
}
impl Memory {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
        }
    }
    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
    fn generation(&self, key: &str) -> u64 {
        self.generations
            .lock()
            .unwrap()
            .get(key)
            .copied()
            .unwrap_or(0)
    }
    fn read(&self, key: &str) -> Vec<Value> {
        let mut history: Vec<Value> = std::fs::read(self.path(key))
            .ok()
            .filter(|bytes| bytes.len() <= MAX_BYTES * 2)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        history.retain(Value::is_object);
        cap_history(&mut history, MAX_TURNS, MAX_BYTES);
        history
    }
    fn load(&self, key: &str) -> Vec<Value> {
        let mut cache = self.cache.lock().unwrap();
        if let Some((history, used)) = cache.get_mut(key) {
            *used = Instant::now();
            return history.clone();
        }
        let history = self.read(key);
        Self::remember(&mut cache, key, history.clone());
        history
    }
    /// Adds a finished turn to the current history (not a stale copy) and saves it, all under
    /// the cache lock. Drops the turn when the memory was cleared since generation `since`.
    fn append(&self, key: &str, turn: Vec<Value>, since: u64) {
        if turn.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().unwrap();
        if self.generation(key) != since {
            return;
        }
        let mut history = match cache.get(key) {
            Some((history, _)) => history.clone(),
            None => self.read(key),
        };
        history.extend(turn);
        cap_history(&mut history, MAX_TURNS, MAX_BYTES);
        if let Ok(bytes) = serde_json::to_vec(&history) {
            let tmp = self.dir.join(format!(".{key}.json.tmp"));
            let _ = std::fs::create_dir_all(&self.dir)
                .and_then(|_| std::fs::write(&tmp, bytes))
                .and_then(|_| std::fs::rename(&tmp, self.path(key)));
        }
        Self::remember(&mut cache, key, history);
    }
    fn clear(&self, key: &str) {
        let mut cache = self.cache.lock().unwrap();
        cache.remove(key);
        *self
            .generations
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_default() += 1;
        let _ = std::fs::remove_file(self.path(key));
    }
    /// Caches a history, evicting the least recently used one when full.
    fn remember(
        cache: &mut HashMap<String, (Vec<Value>, Instant)>,
        key: &str,
        history: Vec<Value>,
    ) {
        if !cache.contains_key(key) && cache.len() >= CACHE_SIZE {
            let oldest = cache
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
        cache.insert(key.to_owned(), (history, Instant::now()));
    }
}

/// Memory file key for a message; Server falls back to the channel outside a server.
fn scope_key(scope: &MemoryScope, channel: u64, guild: Option<u64>, user: u64) -> String {
    match (scope, guild) {
        (MemoryScope::Server, Some(guild)) => format!("server-{guild}"),
        (MemoryScope::User, _) => format!("user-{user}"),
        _ => format!("channel-{channel}"),
    }
}
/// Folder-safe bot name: lowercase ASCII letters, digits, '-' and '_'.
pub(crate) fn sanitize(name: &str) -> String {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    match clean.trim_matches('_') {
        "" => "bot".into(),
        clean => clean.into(),
    }
}
/// Parses Discord IDs from entries that may each hold several, separated by commas or spaces.
fn parse_ids(list: &[String]) -> Result<Vec<u64>> {
    let mut ids = vec![];
    let parts = list
        .iter()
        .flat_map(|s| s.split(|c: char| c == ',' || c.is_whitespace()));
    for part in parts.filter(|s| !s.is_empty()) {
        let id = part
            .parse::<u64>()
            .ok()
            .filter(|&id| id > 0)
            .ok_or_else(|| anyhow!("{part:?} is not a numeric Discord ID"))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}
fn validate_webhook(url: &str) -> Result<()> {
    let url = reqwest::Url::parse(url).map_err(|_| anyhow!("Invalid webhook URL"))?;
    ensure!(
        url.scheme() == "https"
            && matches!(url.host_str(), Some("discord.com" | "discordapp.com"))
            && url.path().starts_with("/api/webhooks/"),
        "Use an official https://discord.com/api/webhooks/… URL"
    );
    Ok(())
}
/// Channels must be listed; an empty server list allows any server.
fn allowed(channels: &[u64], guilds: &[u64], channel: u64, guild: Option<u64>) -> bool {
    channels.contains(&channel) && (guilds.is_empty() || guild.is_some_and(|g| guilds.contains(&g)))
}
/// The trimmed text after `<prefix> `, or None when the message is not for this bot.
fn parse_command<'a>(content: &'a str, prefix: &str) -> Option<&'a str> {
    let command = content.strip_prefix(prefix)?;
    if !command.is_empty() && !command.starts_with(char::is_whitespace) {
        return None;
    }
    Some(command.trim()).filter(|c| !c.is_empty())
}

/// Appends the newest engine turn to the saved history, with the author's name in the user
/// message (and without injected documentation). Later characters in a group reply add only
/// their answer, not the question again.
fn append_turn(
    history: &mut Vec<Value>,
    returned: Vec<Value>,
    speaker_line: &str,
    include_user: bool,
) {
    let Some(start) = returned.iter().rposition(|m| m["role"] == "user") else {
        return;
    };
    let mut turn: Vec<Value> = returned.into_iter().skip(start).collect();
    turn[0]["content"] = json!(speaker_line);
    if !include_user {
        turn.remove(0);
    }
    history.extend(turn);
    cap_history(history, MAX_TURNS, MAX_BYTES);
}
/// Keeps whole turns (each starting at a user message): at most `max_turns`, and drops the
/// oldest turns until the JSON fits in `max_bytes`.
fn cap_history(history: &mut Vec<Value>, max_turns: usize, max_bytes: usize) {
    let first = history
        .iter()
        .position(|m| m["role"] == "user")
        .unwrap_or(history.len());
    history.drain(..first);
    let starts: Vec<usize> = (0..history.len())
        .filter(|&i| history[i]["role"] == "user")
        .collect();
    if starts.len() > max_turns {
        history.drain(..starts[starts.len() - max_turns]);
    }
    while !history.is_empty() && serde_json::to_vec(&*history).map_or(0, |v| v.len()) > max_bytes {
        let next = history
            .iter()
            .skip(1)
            .position(|m| m["role"] == "user")
            .map_or(history.len(), |i| i + 1);
        history.drain(..next);
    }
}

/// Case-insensitive (Unicode, per character) prefix strip.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut chars = text.char_indices();
    for p in prefix.chars() {
        let (_, t) = chars.next()?;
        if !p.to_lowercase().eq(t.to_lowercase()) {
            return None;
        }
    }
    Some(chars.next().map_or("", |(i, _)| &text[i..]))
}
/// Like strip_prefix_ci, but the prefix must end at whitespace or the end of the text.
fn strip_word_ci<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    strip_prefix_ci(text, word)
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Picks the characters (only those available on Discord) and the question for a command:
/// `group <q>`, `@Name <q>` (longest matching name wins), or the bot's default.
fn select_personas(
    command: &str,
    personas: &[Persona],
    group_default: bool,
) -> (Vec<Persona>, String) {
    let usable: Vec<&Persona> = personas.iter().filter(|p| p.discord).collect();
    let group = |question: &str| -> (Vec<Persona>, String) {
        (
            usable
                .iter()
                .filter(|p| p.in_group)
                .take(6)
                .map(|p| (*p).clone())
                .collect(),
            question.trim().into(),
        )
    };
    if let Some(question) = strip_word_ci(command, "group") {
        return group(question);
    }
    if let Some(named) = command.strip_prefix('@') {
        let best = usable
            .iter()
            .filter_map(|p| {
                let name = p.name.trim();
                let rest = strip_word_ci(named, name).filter(|_| !name.is_empty())?;
                Some((name.chars().count(), *p, rest))
            })
            .max_by_key(|(len, _, _)| *len);
        return best.map_or((vec![], String::new()), |(_, p, rest)| {
            (vec![p.clone()], rest.trim().into())
        });
    }
    if group_default {
        group(command)
    } else {
        (
            usable.first().map(|p| (*p).clone()).into_iter().collect(),
            command.trim().into(),
        )
    }
}
fn message_chunks(text: &str) -> Vec<String> {
    let (mut chunks, mut current, mut units) = (vec![], String::new(), 0);
    for ch in text.chars().take(12_000) {
        if units + ch.len_utf16() > 1700 {
            chunks.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += ch.len_utf16();
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
#[cfg(test)]
mod tests {
    use super::*;
    fn persona(name: &str, discord: bool) -> Persona {
        Persona {
            name: name.into(),
            discord,
            ..Default::default()
        }
    }
    #[test]
    fn group_and_named_character_routing() {
        let p = vec![persona("Reviewer", true)];
        assert_eq!(
            select_personas("@Reviewer review this", &p, false).0[0].name,
            "Reviewer"
        );
        assert_eq!(select_personas("group hello", &p, false).0.len(), 1);
        assert!(select_personas("@unknown hello", &p, false).0.is_empty());
    }
    #[test]
    fn routing_matches_longest_unicode_multi_word_name() {
        let p = vec![
            persona("Code", true),
            persona("Code Reviewer", true),
            persona("Élodie", true),
        ];
        let (chosen, question) = select_personas("@code reviewer  check this", &p, false);
        assert_eq!(
            (chosen[0].name.as_str(), question.as_str()),
            ("Code Reviewer", "check this")
        );
        assert_eq!(select_personas("@Code hi", &p, false).0[0].name, "Code");
        assert_eq!(
            select_personas("@ÉLODIE bonjour", &p, false).0[0].name,
            "Élodie"
        );
        assert!(select_personas("@Codex hi", &p, false).0.is_empty());
        assert!(select_personas("@Code", &p, false).1.is_empty());
    }
    #[test]
    fn only_discord_characters_are_used() {
        let p = vec![persona("Private", false), persona("Public", true)];
        assert_eq!(select_personas("hello", &p, false).0[0].name, "Public");
        assert!(select_personas("@Private hi", &p, false).0.is_empty());
        assert_eq!(select_personas("hi", &p, true).0.len(), 1);
        assert!(
            select_personas("hi", &[persona("Private", false)], false)
                .0
                .is_empty()
        );
    }
    #[test]
    fn scope_keys_and_bot_folders() {
        assert_eq!(scope_key(&MemoryScope::Channel, 1, Some(2), 3), "channel-1");
        assert_eq!(scope_key(&MemoryScope::Server, 1, Some(2), 3), "server-2");
        assert_eq!(scope_key(&MemoryScope::Server, 1, None, 3), "channel-1");
        assert_eq!(scope_key(&MemoryScope::User, 1, Some(2), 3), "user-3");
        assert_eq!(sanitize("My Bot/../x"), "my_bot____x");
        assert_eq!(sanitize("  ✨ "), "bot");
    }
    #[test]
    fn commands_ids_and_allowlists() {
        assert_eq!(parse_command("!ai  stop ", "!ai"), Some("stop"));
        assert_eq!(parse_command("!aihello", "!ai"), None);
        assert_eq!(parse_command("!ai", "!ai"), None);
        assert_eq!(
            parse_ids(&["1, 2".into(), "2 3".into()]).unwrap(),
            vec![1, 2, 3]
        );
        assert!(parse_ids(&["abc".into()]).is_err());
        assert!(allowed(&[1], &[], 1, None));
        assert!(!allowed(&[], &[], 1, Some(9)));
        assert!(!allowed(&[1], &[9], 1, Some(8)));
        assert!(allowed(&[1], &[9], 1, Some(9)));
        assert!(validate_webhook("https://discord.com/api/webhooks/1/x").is_ok());
        assert!(validate_webhook("https://discordapp.com/api/webhooks/1/x").is_ok());
        assert!(validate_webhook("https://evil.com/api/webhooks/1/x").is_err());
    }
    #[test]
    fn memory_appends_to_current_history_and_respects_reset() {
        let dir = std::env::temp_dir().join(format!("kyttu-mem-{}", std::process::id()));
        let memory = Memory::new(dir.clone());
        let turn = |q: &str| {
            vec![
                json!({"role":"user","content":q}),
                json!({"role":"assistant","content":"a"}),
            ]
        };
        // Two replies that started from the same history both keep their turns.
        let (a, b) = (memory.generation("k"), memory.generation("k"));
        memory.append("k", turn("one"), a);
        memory.append("k", turn("two"), b);
        assert_eq!(memory.load("k").len(), 4);
        // A reply that started before a reset does not restore the old history.
        let stale = memory.generation("k");
        memory.clear("k");
        memory.append("k", turn("late"), stale);
        assert!(memory.load("k").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn history_keeps_author_names_and_is_capped() {
        let mut history = vec![];
        let returned = vec![
            json!({"role":"user","content":"docs…\nUser question: hi"}),
            json!({"role":"assistant","content":"[A]\nhello"}),
        ];
        append_turn(&mut history, returned.clone(), "Kit: hi", true);
        append_turn(&mut history, returned, "Kit: hi", false);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0]["content"], "Kit: hi");
        let mut long: Vec<Value> = (0..100)
            .flat_map(|i| {
                [
                    json!({"role":"user","content":i}),
                    json!({"role":"assistant","content":"x"}),
                ]
            })
            .collect();
        cap_history(&mut long, 40, MAX_BYTES);
        assert_eq!((long.len(), &long[0]["content"]), (80, &json!(60)));
        cap_history(&mut long, 40, 200);
        assert!(serde_json::to_vec(&long).unwrap().len() <= 200 && long[0]["role"] == "user");
    }
    #[test]
    fn discord_chunks_respect_utf16_limits() {
        let text = "🦊".repeat(2000);
        let chunks = message_chunks(&text);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|s| s.encode_utf16().count() <= 1700));
    }
}

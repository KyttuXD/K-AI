use super::{
    discord_server::{AutoChannels, ImportLimits, ServerStore},
    discord_tools::{DiscordToolHost, GameStore, MusicControl, MusicRequest, ToolContext},
    music::{Music, MusicLimits, Requester},
};
use crate::{
    config::{Config, data_dir},
    core::personas::{Persona, group_order, same_name},
    docs::Library,
    engine::{self, Event, Request, ToolHost},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::{
    all::{
        ChannelId, Command as AppCommand, Context, CreateAllowedMentions, CreateAttachment,
        CreateMessage, EditMessage, EventHandler, ExecuteWebhook, GatewayIntents, GuildId, Message,
        MessageId, Permissions, Ready, User, UserId, Webhook,
    },
    async_trait,
};
use songbird::SerenityInit;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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
/// How often a streaming reply message is edited.
const EDIT_INTERVAL: Duration = Duration::from_millis(1500);
/// Changed memory is saved this long after the first change (several replies share a write).
const FLUSH_DELAY: Duration = Duration::from_secs(2);
const POST_FAILED: &str = "Could not post. Check bot/webhook permissions and rate limits.";
/// Auto-reply channels: the gap between two replies in one channel, and between two replies
/// to other bots.
const AUTO_CHANNEL_COOLDOWN: Duration = Duration::from_secs(3);
const BOT_COOLDOWN: Duration = Duration::from_secs(10);
/// Bot messages with more mentions than this are never answered.
const MAX_BOT_MENTIONS: usize = 5;
/// How long an auto reply waits for the channel (or the bot) to be free before giving up.
const AUTO_WAIT: Duration = Duration::from_secs(300);
/// Per-server files (auto channels, server characters) are re-read after this long, so
/// removals made in the app reach a running bot.
const SERVER_FILES_TTL: Duration = Duration::from_secs(5);
/// Written once old slash commands were removed, so the cleanup runs only once per bot.
const SLASH_MARKER: &str = ".slash_removed";

/// Which conversation a bot remembers: per channel, per server or per user.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum MemoryScope {
    #[default]
    Channel,
    Server,
    User,
}

/// Music tool limits.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MusicSettings {
    /// Role name or ID needed for skip/stop/clear (empty = anyone).
    pub dj_role: String,
    pub max_queue_per_user: usize,
    pub max_track_minutes: u32,
}
impl Default for MusicSettings {
    fn default() -> Self {
        Self {
            dj_role: String::new(),
            max_queue_per_user: 5,
            max_track_minutes: 15,
        }
    }
}

/// Which moderation actions the model may take, and where they are logged.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModSettings {
    /// Mod-log channel ID (empty = no log).
    pub log_channel: String,
    pub allow_delete: bool,
    pub allow_timeout: bool,
    pub allow_kick: bool,
    pub allow_ban: bool,
    pub allow_roles: bool,
}

/// Tool groups the model can use on Discord. All off by default.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BotTools {
    pub music: bool,
    pub moderation: bool,
    pub games: bool,
    pub music_tools: MusicSettings,
    pub moderation_settings: ModSettings,
    /// Games offered (empty = all).
    pub games_list: Vec<String>,
}
impl BotTools {
    pub fn any(&self) -> bool {
        self.music || self.moderation || self.games
    }
}

/// One Discord bot's settings. Its token and webhooks are kept in memory, or in the OS
/// keychain when `remember` is on; never in settings.json.
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
    pub remember: bool,
    pub tools: BotTools,
    /// Admins can turn on channels where every message gets a reply (`<prefix> auto on`).
    pub auto_channels_enabled: bool,
    /// In auto channels, also answer other bots and webhooks (never this app's own bots).
    pub bot_replies: bool,
    /// Bot-triggered replies in a row per channel, until a person posts.
    pub bot_reply_limit: u32,
    /// Admins can import characters for their own server (`<prefix> character import`).
    pub server_characters: bool,
    pub server_char_limit: usize,
    pub server_char_max_kb: usize,
    /// Answer as the plain agent even when characters are available on Discord.
    pub no_character: bool,
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
            remember: false,
            tools: BotTools::default(),
            auto_channels_enabled: false,
            bot_replies: false,
            bot_reply_limit: 3,
            server_characters: false,
            server_char_limit: 10,
            server_char_max_kb: 32,
            no_character: false,
        }
    }
}

/// Keychain keys for a bot's token and webhook lines.
pub fn token_key(bot: &str) -> String {
    format!("discord-token-{}", sanitize(bot))
}
pub fn webhooks_key(bot: &str) -> String {
    format!("discord-webhooks-{}", sanitize(bot))
}

/// User IDs and webhook IDs of every bot this app is running, so bots never answer each
/// other (or themselves) in auto channels. An ID is listed once per running bot that uses it.
static APP_BOTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static APP_WEBHOOKS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// True when `user` is a bot account run by this app.
pub fn is_app_bot(user: u64) -> bool {
    APP_BOTS.lock().unwrap().contains(&user)
}
/// True when `webhook` is one of this app's character webhooks.
pub fn is_app_webhook(webhook: u64) -> bool {
    APP_WEBHOOKS.lock().unwrap().contains(&webhook)
}
fn register_ids(list: &Mutex<Vec<u64>>, ids: &[u64]) {
    list.lock()
        .unwrap()
        .extend(ids.iter().filter(|&&id| id > 0));
}
fn unregister_ids(list: &Mutex<Vec<u64>>, ids: &[u64]) {
    let mut list = list.lock().unwrap();
    for id in ids {
        if let Some(i) = list.iter().position(|x| x == id) {
            list.swap_remove(i);
        }
    }
}

fn music_limits(settings: &MusicSettings) -> MusicLimits {
    MusicLimits {
        dj_role: settings.dj_role.trim().to_owned(),
        max_queue_per_user: settings.max_queue_per_user,
        max_track_minutes: settings.max_track_minutes,
    }
}

/// Connects the model's music tools (discord_tools) to this bot's player (music.rs). The tool
/// host has already decided whether the requester counts as DJ.
struct MusicBridge {
    music: Arc<Music>,
    ctx: Context,
    limits: MusicLimits,
}
impl MusicBridge {
    fn requester(req: &MusicRequest) -> Requester {
        Requester {
            user: req.requester.get(),
            name: req.requester_name.clone(),
            roles: vec![],
            admin: req.is_dj,
        }
    }
}
#[async_trait]
impl MusicControl for MusicBridge {
    async fn play(&self, req: &MusicRequest, query: &str) -> Result<String> {
        let requester = Self::requester(req);
        self.music
            .play(
                &self.ctx,
                req.guild,
                req.voice_channel,
                &requester,
                query,
                &self.limits,
            )
            .await
    }
    async fn skip(&self, req: &MusicRequest) -> Result<String> {
        let requester = Self::requester(req);
        self.music.skip(req.guild, &requester, &self.limits).await
    }
    async fn stop(&self, req: &MusicRequest) -> Result<String> {
        let requester = Self::requester(req);
        self.music.stop(req.guild, &requester, &self.limits).await
    }
    async fn pause(&self, req: &MusicRequest) -> Result<String> {
        let requester = Self::requester(req);
        self.music.pause(req.guild, &requester, &self.limits).await
    }
    async fn resume(&self, req: &MusicRequest) -> Result<String> {
        let requester = Self::requester(req);
        self.music.resume(req.guild, &requester, &self.limits).await
    }
    async fn queue(&self, req: &MusicRequest) -> Result<String> {
        Ok(self.music.queue_text(req.guild).await)
    }
}

/// A bot's stop request; `wake` lets the shutdown watcher sleep instead of polling.
#[derive(Default)]
struct StopSignal {
    flag: AtomicBool,
    wake: tokio::sync::Notify,
}
impl StopSignal {
    fn set(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }
    fn is_set(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

/// One bot session. Several can run at once; they share only the registry of this app's
/// bot and webhook IDs.
pub struct Discord {
    pub status: Arc<Mutex<String>>,
    pub running: Arc<AtomicBool>,
    stop: Mutex<Arc<StopSignal>>,
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// App characters available on Discord for the bot that last connected.
    available: Arc<AtomicUsize>,
}
impl Default for Discord {
    fn default() -> Self {
        Self {
            status: Arc::new(Mutex::new("Disconnected".into())),
            running: Arc::new(AtomicBool::new(false)),
            stop: Mutex::new(Arc::default()),
            replies: Arc::new(Mutex::new(HashMap::new())),
            available: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Discord {
    /// How many characters this bot can answer as on Discord (0 = it answers as the plain agent).
    pub fn available_characters(&self) -> usize {
        self.available.load(Ordering::Relaxed)
    }
    pub fn disconnect(&self) {
        self.stop.lock().unwrap().set();
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
        ensure!(
            !channels.is_empty() || profile.auto_channels_enabled,
            "List at least one channel ID"
        );
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
        let stop = Arc::new(StopSignal::default());
        *self.stop.lock().unwrap() = stop.clone();
        self.replies.lock().unwrap().clear();
        let running = self.running.clone();
        let status = self.status.clone();
        *status.lock().unwrap() = "Connecting…".into();
        let memory = Arc::new(Memory::new(
            data_dir().join("discord").join(sanitize(&profile.name)),
        ));
        let hook_ids: Vec<u64> = hooks.values().filter_map(|url| webhook_id(url)).collect();
        register_ids(&APP_WEBHOOKS, &hook_ids);
        let own_id = Arc::new(AtomicU64::new(0));
        let characters = if profile.no_character {
            0
        } else {
            config.personas.iter().filter(|p| p.discord).count()
        };
        self.available.store(characters, Ordering::Relaxed);
        let (use_voice, use_reactions) = (profile.tools.music, profile.tools.moderation);
        let mut handler = Handler {
            memory: memory.clone(),
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
            own_id: own_id.clone(),
            music: None,
            games: Arc::new(GameStore::default()),
            auto_channels: Mutex::new(None),
            auto_state: Mutex::new(HashMap::new()),
            server_chars: Mutex::new(HashMap::new()),
        };
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            if let Ok(runtime) = result {
                // The player (and its songbird instance) is made inside the bot's runtime.
                if use_voice {
                    let _enter = runtime.enter();
                    handler.music = Some(Music::new());
                }
                runtime.block_on(run_client(
                    token,
                    handler,
                    status.clone(),
                    stop,
                    use_reactions,
                ));
            } else {
                *status.lock().unwrap() = "Could not initialize Discord runtime".into();
            }
            memory.flush();
            unregister_ids(&APP_WEBHOOKS, &hook_ids);
            unregister_ids(&APP_BOTS, &[own_id.load(Ordering::SeqCst)]);
            running.store(false, Ordering::Relaxed);
        });
        Ok(())
    }
}

/// Gateway events the bot needs: voice states only with music, reactions only with
/// moderation (confirmation buttons).
fn intents(voice: bool, reactions: bool) -> GatewayIntents {
    let mut intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    if voice {
        intents |= GatewayIntents::GUILD_VOICE_STATES;
    }
    if reactions {
        intents |= GatewayIntents::GUILD_MESSAGE_REACTIONS;
    }
    intents
}

async fn run_client(
    token: String,
    handler: Handler,
    status: Arc<Mutex<String>>,
    stop: Arc<StopSignal>,
    reactions: bool,
) {
    // Voice (this bot's songbird instance) is registered only for a bot with music on.
    let music = handler.music.clone();
    let mut builder = serenity::Client::builder(&token, intents(music.is_some(), reactions));
    if let Some(music) = &music {
        builder = builder.register_songbird_with(music.songbird());
    }
    let mut client = match builder.event_handler(handler).await {
        Ok(client) => client,
        Err(_) => {
            *status.lock().unwrap() = "Could not connect. Check your bot token and network.".into();
            return;
        }
    };
    if stop.is_set() {
        *status.lock().unwrap() = "Disconnected".into();
        return;
    }
    let manager = client.shard_manager.clone();
    let stopping = stop.clone();
    // notify_one keeps a permit, so a stop that happened before this waits is not missed.
    let watcher = tokio::spawn(async move {
        stopping.wake.notified().await;
        if let Some(music) = music {
            music.shutdown().await;
        }
        manager.shutdown_all().await;
    });
    *status.lock().unwrap() = if client.start().await.is_err() {
        "Connection failed. Check token, Message Content Intent, channel access, and network."
            .into()
    } else {
        "Disconnected".into()
    };
    stop.flag.store(true, Ordering::Relaxed);
    watcher.abort();
}

/// Music prefix commands (the same player the model's music tools use).
#[derive(Debug, PartialEq)]
enum MusicCmd {
    Play(String),
    Skip,
    Pause,
    Resume,
    Queue,
    /// Stops the music, clears the queue and leaves the voice channel.
    Leave,
}
/// `auto on|off|list`.
#[derive(Debug, PartialEq)]
enum AutoCmd {
    On,
    Off,
    List,
}
/// `character import|list|remove <name>|export <name>`.
#[derive(Debug, PartialEq)]
enum CharacterCmd {
    Import,
    List,
    Remove(String),
    Export(String),
}

/// What a user asked for in a prefixed message (or an auto-channel message).
#[derive(Debug, PartialEq)]
enum Command {
    /// Stops the running reply; with music on and no reply running, stops the music.
    Stop,
    Reset,
    /// Every group character answers.
    Group(String),
    /// The default character(s), or `@Name question`.
    Ask(String),
    Music(MusicCmd),
    Auto(AutoCmd),
    Character(CharacterCmd),
}

/// Which optional prefix commands this bot understands.
#[derive(Clone, Copy, Debug, Default)]
struct Features {
    music: bool,
    auto: bool,
    characters: bool,
}

/// Why a reply runs: a prefixed message, an auto channel, and whether a bot or webhook asked.
#[derive(Clone, Copy, Debug, Default)]
struct Trigger {
    auto: bool,
    from_bot: bool,
}

/// Whether `run` handled the command or found the channel (or the bot) busy.
#[derive(Debug, PartialEq)]
enum Outcome {
    Done,
    Busy,
}

/// An auto-channel message waiting for the channel to be free (only the latest is kept).
struct Pending {
    msg: Box<Message>,
    text: String,
    from_bot: bool,
}

/// One auto channel's reply state.
#[derive(Default)]
struct AutoState {
    /// A task is answering this channel (and will take `pending` next).
    busy: bool,
    pending: Option<Pending>,
    last_reply: Option<Instant>,
    last_bot_reply: Option<Instant>,
    /// Bot-triggered replies in a row; a person's message resets it.
    bot_chain: u32,
}
impl AutoState {
    /// How long until the next reply may start: the channel cooldown, plus the bot cooldown
    /// when a bot asked.
    fn wait(&self, from_bot: bool, now: Instant) -> Duration {
        let left = |since: Option<Instant>, gap: Duration| {
            since.map_or(Duration::ZERO, |t| {
                gap.saturating_sub(now.duration_since(t))
            })
        };
        let wait = left(self.last_reply, AUTO_CHANNEL_COOLDOWN);
        if from_bot {
            wait.max(left(self.last_bot_reply, BOT_COOLDOWN))
        } else {
            wait
        }
    }
}

struct Handler {
    config: Config,
    library: Library,
    profile: BotProfile,
    channels: Vec<u64>,
    guilds: Vec<u64>,
    webhooks: HashMap<u64, String>,
    status: Arc<Mutex<String>>,
    stop: Arc<StopSignal>,
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    cooldowns: Mutex<HashMap<u64, Instant>>,
    memory: Arc<Memory>,
    /// This bot's user ID, once connected.
    own_id: Arc<AtomicU64>,
    music: Option<Arc<Music>>,
    games: Arc<GameStore>,
    /// Channels with auto replies on, and when they were read.
    auto_channels: Mutex<Option<(Instant, AutoChannels)>>,
    auto_state: Mutex<HashMap<u64, AutoState>>,
    /// Server characters per guild, and when they were read.
    server_chars: Mutex<HashMap<u64, (Instant, Vec<Persona>)>>,
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
/// Frees an auto channel if its task ends early (stop, panic); a normal finish frees it under
/// the lock itself and sets `done`.
struct AutoBusy<'a> {
    states: &'a Mutex<HashMap<u64, AutoState>>,
    channel: u64,
    done: bool,
}
impl Drop for AutoBusy<'_> {
    fn drop(&mut self) {
        if !self.done
            && let Ok(mut states) = self.states.lock()
            && let Some(state) = states.get_mut(&self.channel)
        {
            state.busy = false;
            state.pending = None;
        }
    }
}

/// Where one request's output goes: the channel the message came from.
struct Out<'a> {
    ctx: &'a Context,
    channel: ChannelId,
}
// serenity::Error is serenity's own type; boxing it here would only move the allocation.
#[allow(clippy::result_large_err)]
impl<'a> Out<'a> {
    fn new(ctx: &'a Context, channel: ChannelId) -> Self {
        Self { ctx, channel }
    }
    /// A short notice in the channel.
    async fn say(&self, text: &str) {
        let _ = self.post(text.to_owned()).await;
    }
    async fn post(&self, content: String) -> serenity::Result<MessageId> {
        let message = CreateMessage::new()
            .content(content)
            .allowed_mentions(no_mentions());
        let sent = self.channel.send_message(&self.ctx.http, message).await?;
        Ok(sent.id)
    }
    async fn edit(&self, id: MessageId, content: String) -> serenity::Result<()> {
        let edit = EditMessage::new()
            .content(content)
            .allowed_mentions(no_mentions());
        self.channel
            .edit_message(&self.ctx.http, id, edit)
            .await
            .map(|_| ())
    }
}

/// Posts or edits the messages of one streamed answer so they show `text`; `stream` holds the
/// messages already posted and what each shows. Earlier chunks stop changing once full.
#[allow(clippy::result_large_err)]
async fn show(
    out: &Out<'_>,
    stream: &mut Vec<(MessageId, String)>,
    name: &str,
    text: &str,
) -> serenity::Result<()> {
    for (i, chunk) in message_chunks(text).into_iter().enumerate() {
        let content = format!("**{name}**\n{chunk}");
        match stream.get_mut(i) {
            Some((_, shown)) if *shown == content => {}
            Some((posted, shown)) => {
                out.edit(*posted, content.clone()).await?;
                *shown = content;
            }
            None => {
                let posted = out.post(content.clone()).await?;
                stream.push((posted, content));
            }
        }
    }
    Ok(())
}

/// A member's permissions in a channel, read over HTTP (the bot keeps no cache).
async fn member_permissions(
    ctx: &Context,
    guild: GuildId,
    channel: ChannelId,
    user: UserId,
) -> Option<Permissions> {
    let partial = guild.to_partial_guild(ctx).await.ok()?;
    let mut channel = channel.to_channel(ctx).await.ok()?.guild()?;
    // A thread has no overwrites of its own; its parent channel's apply.
    if channel.thread_metadata.is_some()
        && let Some(parent) = channel.parent_id
        && let Some(parent) = parent.to_channel(ctx).await.ok().and_then(|c| c.guild())
    {
        channel = parent;
    }
    let member = guild.member(ctx, user).await.ok()?;
    Some(partial.user_permissions_in(&channel, &member))
}
fn has_permission(permissions: Permissions, needed: Permissions) -> bool {
    permissions.administrator() || permissions.contains(needed)
}
/// Shortens a tool or player message for one Discord post.
fn short(text: &str) -> String {
    text.trim().chars().take(1900).collect()
}

impl Handler {
    fn set_status(&self, text: impl Into<String>) {
        *self.status.lock().unwrap() = text.into();
    }
    /// "N characters available on Discord" for the status line (app characters only).
    fn characters_note(&self) -> String {
        let n = if self.profile.no_character {
            0
        } else {
            self.config.personas.iter().filter(|p| p.discord).count()
        };
        match n {
            1 => "1 character available on Discord".into(),
            n => format!("{n} characters available on Discord"),
        }
    }
    fn features(&self) -> Features {
        Features {
            music: self.profile.tools.music,
            auto: self.profile.auto_channels_enabled,
            characters: self.profile.server_characters,
        }
    }
    fn store(&self) -> ServerStore {
        ServerStore::for_bot(&self.profile.name)
    }
    /// An empty server list allows every server.
    fn guild_allowed(&self, guild: Option<u64>) -> bool {
        guild.is_some_and(|g| self.guilds.is_empty() || self.guilds.contains(&g))
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
    /// Saves changed memory a little later, so replies close together share one write.
    fn schedule_flush(&self) {
        if !self.memory.flush_pending.swap(true, Ordering::SeqCst) {
            let memory = self.memory.clone();
            tokio::spawn(async move {
                tokio::time::sleep(FLUSH_DELAY).await;
                let _ = tokio::task::spawn_blocking(move || memory.flush()).await;
            });
        }
    }
    /// True when auto replies are on in this channel (re-read every few seconds, so a
    /// removal in the app reaches the running bot).
    fn is_auto_channel(&self, guild: u64, channel: u64) -> bool {
        let mut cache = self.auto_channels.lock().unwrap();
        if cache
            .as_ref()
            .is_none_or(|(read, _)| read.elapsed() >= SERVER_FILES_TTL)
        {
            *cache = Some((Instant::now(), self.store().auto_channels()));
        }
        cache
            .as_ref()
            .is_some_and(|(_, auto)| auto.contains(guild, channel))
    }
    /// A server's own characters (discord_server keeps them on Discord, without a model).
    fn server_personas(&self, guild: u64) -> Vec<Persona> {
        let mut cache = self.server_chars.lock().unwrap();
        if let Some((read, list)) = cache.get(&guild)
            && read.elapsed() < SERVER_FILES_TTL
        {
            return list.clone();
        }
        let mut list = self.store().characters(guild);
        for persona in &mut list {
            persona.discord = true;
            persona.model.clear();
        }
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(guild, (Instant::now(), list.clone()));
        list
    }
    /// The app's characters plus, in a server with server characters on, that server's own
    /// (an app character keeps its name when both use it).
    fn personas_for(&self, guild: Option<u64>) -> Vec<Persona> {
        let mut all = self.config.personas.clone();
        if self.profile.server_characters
            && let Some(guild) = guild
        {
            let extra: Vec<Persona> = self
                .server_personas(guild)
                .into_iter()
                .filter(|s| !all.iter().any(|p| p.discord && same_name(&p.name, &s.name)))
                .collect();
            all.extend(extra);
        }
        all
    }
    /// Whether the author has `needed` in this channel (Administrator always passes).
    async fn member_can(&self, ctx: &Context, msg: &Message, needed: Permissions) -> bool {
        let Some(guild) = msg.guild_id else {
            return false;
        };
        member_permissions(ctx, guild, msg.channel_id, msg.author.id)
            .await
            .is_some_and(|permissions| has_permission(permissions, needed))
    }
    /// The model's tools for one message from a person, when a tool group is on. The host
    /// checks the member's and the bot's Discord permissions itself on every call.
    fn tool_host(
        &self,
        ctx: &Context,
        msg: &Message,
        cancel: &Arc<AtomicBool>,
    ) -> Option<Arc<DiscordToolHost>> {
        if !self.profile.tools.any() {
            return None;
        }
        let music = self.music.as_ref().map(|music| {
            Arc::new(MusicBridge {
                music: music.clone(),
                ctx: ctx.clone(),
                limits: music_limits(&self.profile.tools.music_tools),
            }) as Arc<dyn MusicControl>
        });
        let cx = ToolContext {
            http: ctx.http.clone(),
            runtime: tokio::runtime::Handle::current(),
            guild: msg.guild_id,
            channel: msg.channel_id,
            trigger: Some(msg.id),
            requester: msg.author.id,
            requester_name: speaker(&msg.author),
            voice_channel: None,
            tools: self.profile.tools.clone(),
            music,
            games: self.games.clone(),
            cancel: cancel.clone(),
        };
        Some(Arc::new(DiscordToolHost::new(cx)))
    }
    /// Music prefix commands, run on this bot's player with the same DJ and queue rules as
    /// the model's music tools.
    async fn music_command(&self, out: &Out<'_>, msg: &Message, cmd: MusicCmd) {
        let (Some(music), Some(guild)) = (&self.music, msg.guild_id) else {
            out.say("Music works in servers only.").await;
            return;
        };
        let limits = music_limits(&self.profile.tools.music_tools);
        let name = speaker(&msg.author);
        let requester = Requester::fetch(out.ctx, guild, msg.author.id, &name, &limits).await;
        let result = match cmd {
            MusicCmd::Play(query) if query.trim().is_empty() => {
                out.say("Add a song name or link.").await;
                return;
            }
            MusicCmd::Play(query) => {
                let _ = msg.channel_id.broadcast_typing(&out.ctx.http).await;
                music
                    .play(out.ctx, guild, None, &requester, query.trim(), &limits)
                    .await
            }
            MusicCmd::Skip => music.skip(guild, &requester, &limits).await,
            MusicCmd::Pause => music.pause(guild, &requester, &limits).await,
            MusicCmd::Resume => music.resume(guild, &requester, &limits).await,
            MusicCmd::Queue => Ok(music.queue_text(guild).await),
            MusicCmd::Leave => music.leave(guild, &requester, &limits).await,
        };
        let text = match result {
            Ok(text) => text,
            Err(e) => format!("{e:#}"),
        };
        out.say(&short(&text)).await;
    }
    /// `auto on|off|list`: admins (Manage Channels) choose channels where every message
    /// gets a reply.
    async fn auto_command(&self, out: &Out<'_>, msg: &Message, cmd: AutoCmd) {
        let Some(guild) = msg.guild_id.map(|g| g.get()) else {
            out.say("Auto-reply works in servers only.").await;
            return;
        };
        if !self
            .member_can(out.ctx, msg, Permissions::MANAGE_CHANNELS)
            .await
        {
            out.say("You need Manage Channels to change auto-reply.")
                .await;
            return;
        }
        let channel = msg.channel_id.get();
        let store = self.store();
        let on = match cmd {
            AutoCmd::List => {
                let list: Vec<String> = store
                    .auto_channels()
                    .channels(guild)
                    .iter()
                    .map(|c| format!("<#{c}>"))
                    .collect();
                out.say(&if list.is_empty() {
                    "No auto-reply channels in this server.".to_owned()
                } else {
                    format!("Auto-reply channels: {}", list.join(", "))
                })
                .await;
                return;
            }
            AutoCmd::On => true,
            AutoCmd::Off => false,
        };
        let text = match store.set_auto_channel(guild, channel, on) {
            Ok(true) if on => "Auto-reply is on: I'll answer every message in this channel.",
            Ok(true) => "Auto-reply is off in this channel.",
            Ok(false) if on => "Auto-reply is already on here.",
            Ok(false) => "Auto-reply is not on here.",
            Err(e) => {
                out.say(&short(&format!("Could not change auto-reply: {e}")))
                    .await;
                return;
            }
        };
        *self.auto_channels.lock().unwrap() = None;
        out.say(text).await;
    }
    /// `character import|list|remove|export`: admins (Manage Server) manage this server's own
    /// characters.
    async fn character_command(&self, out: &Out<'_>, msg: &Message, cmd: CharacterCmd) {
        let Some(guild) = msg.guild_id.map(|g| g.get()) else {
            out.say("Server characters work in servers only.").await;
            return;
        };
        if !self
            .member_can(out.ctx, msg, Permissions::MANAGE_GUILD)
            .await
        {
            out.say("You need Manage Server to manage server characters.")
                .await;
            return;
        }
        let store = self.store();
        let result: Result<String> = match cmd {
            CharacterCmd::List => {
                let list = store.characters(guild);
                let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
                out.say(&short(&if names.is_empty() {
                    "No server characters yet.".to_owned()
                } else {
                    format!(
                        "Server characters ({}/{}): {}",
                        names.len(),
                        self.profile.server_char_limit,
                        names.join(", ")
                    )
                }))
                .await;
                return;
            }
            CharacterCmd::Export(name) => {
                let (file_name, bytes) = match store.export_character(guild, &name) {
                    Ok(file) => file,
                    Err(e) => {
                        out.say(&short(&e.to_string())).await;
                        return;
                    }
                };
                let message = CreateMessage::new()
                    .content(format!("**{}**", name.trim()))
                    .add_file(CreateAttachment::bytes(bytes, file_name))
                    .allowed_mentions(no_mentions());
                if msg
                    .channel_id
                    .send_message(&out.ctx.http, message)
                    .await
                    .is_err()
                {
                    out.say("Could not upload the file.").await;
                }
                return;
            }
            CharacterCmd::Remove(name) => match store.remove_character(guild, &name) {
                Ok(Some(removed)) => Ok(format!("Removed **{}**.", removed.name)),
                Ok(None) => Ok("No server character has that name.".into()),
                Err(e) => Err(e),
            },
            CharacterCmd::Import => {
                let max_kb = self
                    .profile
                    .server_char_max_kb
                    .clamp(1, super::discord_server::MAX_CHAR_KB);
                let Some(file) = msg.attachments.first() else {
                    out.say("Attach a character .json file to the command.")
                        .await;
                    return;
                };
                if file.size as usize > max_kb.saturating_mul(1024) {
                    out.say(&format!("That file is larger than {max_kb} KB."))
                        .await;
                    return;
                }
                let Ok(bytes) = file.download().await else {
                    out.say("Could not download the file.").await;
                    return;
                };
                // The bot's own Discord characters keep their names, so `@Name` stays clear.
                let mut limits = ImportLimits::new(self.profile.server_char_limit, max_kb);
                limits.reserved = self
                    .config
                    .personas
                    .iter()
                    .filter(|p| p.discord)
                    .map(|p| p.name.clone())
                    .collect();
                store
                    .import_character(guild, &bytes, &limits)
                    .map(|p| format!("Imported **{}**. Ask with `@{}`.", p.name, p.name))
            }
        };
        self.server_chars.lock().unwrap().remove(&guild);
        let text = match result {
            Ok(text) => text,
            Err(e) => format!("Could not do that: {e}"),
        };
        out.say(&short(&text)).await;
    }
    /// `stop`: the running reply, else (with music on) the music.
    async fn stop_command(&self, out: &Out<'_>, msg: &Message) {
        let flag = self
            .replies
            .lock()
            .unwrap()
            .get(&out.channel.get())
            .cloned();
        if let Some(flag) = flag {
            flag.store(true, Ordering::Relaxed);
            out.say("Stopping the current reply.").await;
            return;
        }
        if let (Some(music), Some(guild)) = (&self.music, msg.guild_id) {
            let limits = music_limits(&self.profile.tools.music_tools);
            let name = speaker(&msg.author);
            let requester = Requester::fetch(out.ctx, guild, msg.author.id, &name, &limits).await;
            let text = match music.stop(guild, &requester, &limits).await {
                Ok(text) => text,
                Err(e) => format!("{e:#}"),
            };
            out.say(&short(&text)).await;
            return;
        }
        out.say("Nothing is running in this channel.").await;
    }
    /// Runs one command from `msg` in a channel the caller allowed. Auto replies skip the
    /// per-user cooldown (the caller applies it) and stay quiet when they cannot answer.
    async fn run(
        &self,
        out: &Out<'_>,
        command: Command,
        msg: &Message,
        trigger: Trigger,
    ) -> Outcome {
        let channel = out.channel.get();
        let guild = msg.guild_id.map(|g| g.get());
        let user = msg.author.id.get();
        if !trigger.auto && !self.cooldown_ok(user) {
            return Outcome::Done;
        }
        let key = scope_key(&self.profile.memory, channel, guild, user);
        match command {
            Command::Stop => {
                self.stop_command(out, msg).await;
                return Outcome::Done;
            }
            Command::Reset => {
                self.memory.clear(&key);
                out.say("Memory cleared.").await;
                return Outcome::Done;
            }
            Command::Music(cmd) => {
                self.music_command(out, msg, cmd).await;
                return Outcome::Done;
            }
            Command::Auto(cmd) => {
                self.auto_command(out, msg, cmd).await;
                return Outcome::Done;
            }
            Command::Character(cmd) => {
                self.character_command(out, msg, cmd).await;
                return Outcome::Done;
            }
            Command::Group(_) | Command::Ask(_) => {}
        }
        let personas = self.personas_for(guild);
        // No characters (or the bot is set to none): the plain agent answers, never a notice.
        let plain = self.profile.no_character || !personas.iter().any(|p| p.discord);
        let (selected, question) = if plain {
            plain_selection(&command)
        } else {
            select_personas(&command, &personas, self.profile.group)
        };
        if question.is_empty() {
            return Outcome::Done;
        }
        if selected.is_empty() {
            if !trigger.auto {
                out.say("No Discord character has that name.").await;
            }
            return Outcome::Done;
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
            if trigger.auto {
                return Outcome::Busy;
            }
            out.say("Busy with another reply. Try again shortly.").await;
            return Outcome::Done;
        }
        let _active = ActiveReply {
            replies: self.replies.clone(),
            channel,
        };
        let webhook = match self.webhooks.get(&channel) {
            None => None,
            Some(url) => match Webhook::from_url(&out.ctx.http, url).await {
                Ok(hook) if hook.channel_id == Some(out.channel) => Some(hook),
                _ => {
                    self.set_status("Webhook validation failed or it belongs to a different channel. Nothing was posted.");
                    out.say("This channel's webhook is not usable.").await;
                    return Outcome::Done;
                }
            },
        };
        // Tools act for the person who asked; a bot or webhook never gets them.
        let host: Option<Arc<dyn ToolHost>> = if trigger.from_bot {
            None
        } else {
            self.tool_host(out.ctx, msg, &cancel)
                .map(|host| host as Arc<dyn ToolHost>)
        };
        let tag = if trigger.from_bot { " (bot)" } else { "" };
        let speaker_line = format!("{}{tag}: {question}", speaker(&msg.author));
        // Read the generation first so a reset during this reply drops its turn.
        let since = self.memory.generation(&key);
        let history = self.memory.load(&key);
        let mut new_turn = vec![];
        let mut failed = false;
        for (index, persona) in selected.iter().enumerate() {
            if self.stop.is_set() || cancel.load(Ordering::Relaxed) {
                break;
            }
            let name = display_name(persona, &self.profile.name);
            self.set_status(format!("{name} is responding…"));
            let config = self.config.clone();
            let persona_model = persona.model.clone();
            let mut request = Request {
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
                tool_host: host.clone(),
            };
            let (bot_stop, limit) = (self.stop.clone(), self.profile.reply_timeout_secs);
            let live = Arc::new(Mutex::new(String::new()));
            let worker_live = live.clone();
            let mut job = tokio::task::spawn_blocking(move || {
                // Only use a character's own model while the server actually serves it.
                // The model list is a blocking HTTP call, so it runs here, off the runtime.
                if !persona_model.is_empty()
                    && crate::core::provider::models(&request.config)
                        .is_ok_and(|served| served.contains(&persona_model))
                {
                    request.config.model = persona_model;
                }
                run_reply(request, bot_stop, limit, worker_live)
            });
            // Webhook posts cannot be edited by the bot, so they get one final post instead.
            let mut stream = vec![];
            let generated = loop {
                match tokio::time::timeout(EDIT_INTERVAL, &mut job).await {
                    Ok(done) => break done,
                    Err(_) if webhook.is_none() && !failed => {
                        let text = live.lock().unwrap().clone();
                        if show(out, &mut stream, &name, &text).await.is_err() {
                            failed = true;
                            cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {}
                }
            };
            if failed {
                self.set_status(POST_FAILED);
                break;
            }
            let Ok((text, result, timed_out)) = generated else {
                self.set_status("Local model worker crashed.");
                break;
            };
            if timed_out {
                let text = format!("{name} hit the {limit}s time limit and was stopped.");
                out.say(&text).await;
                break;
            }
            if self.stop.is_set() || cancel.load(Ordering::Relaxed) {
                break;
            }
            let returned = match result {
                Ok(returned) => returned,
                Err(e) => {
                    self.set_status(format!(
                        "Local model request failed: {}",
                        e.chars().take(200).collect::<String>()
                    ));
                    out.say("Sorry, the local model could not answer.").await;
                    break;
                }
            };
            append_turn(&mut new_turn, returned, &speaker_line, index == 0);
            let posted = match &webhook {
                Some(hook) => {
                    let mut ok = true;
                    for chunk in message_chunks(&text) {
                        let mut builder = ExecuteWebhook::new()
                            .content(chunk)
                            .username(name.clone())
                            .allowed_mentions(no_mentions());
                        if persona.avatar_url.starts_with("https://") {
                            builder = builder.avatar_url(&persona.avatar_url);
                        }
                        if hook.execute(&out.ctx.http, false, builder).await.is_err() {
                            ok = false;
                            break;
                        }
                    }
                    ok
                }
                None => show(out, &mut stream, &name, &text).await.is_ok(),
            };
            if !posted {
                self.set_status(POST_FAILED);
                failed = true;
                break;
            }
        }
        if !new_turn.is_empty() {
            self.memory.append(&key, new_turn, since);
            self.schedule_flush();
        }
        if !failed && !self.stop.is_set() {
            self.set_status(format!(
                "Connected · waiting for a message · {}",
                self.characters_note()
            ));
        }
        Outcome::Done
    }
    /// A message in an auto channel: answered without a prefix, at most one reply at a time
    /// per channel with cooldowns; while busy only the newest message waits.
    async fn auto_message(&self, ctx: &Context, msg: &Message, from_bot: bool) {
        let own = self.own_id.load(Ordering::SeqCst);
        let Some(text) = auto_text(&msg.content, own) else {
            return;
        };
        let channel = msg.channel_id.get();
        if from_bot {
            if !self.profile.bot_replies
                || !bot_message_ok(
                    &msg.content,
                    msg.mention_everyone,
                    msg.mentions.len() + msg.mention_roles.len(),
                )
            {
                return;
            }
        } else if !self.cooldown_ok(msg.author.id.get()) {
            return;
        }
        let pending = Pending {
            msg: Box::new(msg.clone()),
            text,
            from_bot,
        };
        let mut next = {
            let mut states = self.auto_state.lock().unwrap();
            let state = states.entry(channel).or_default();
            if from_bot {
                if state.bot_chain >= self.profile.bot_reply_limit {
                    return;
                }
            } else {
                state.bot_chain = 0;
            }
            if state.busy {
                state.pending = Some(pending);
                return;
            }
            state.busy = true;
            pending
        };
        let mut guard = AutoBusy {
            states: &self.auto_state,
            channel,
            done: false,
        };
        let out = Out::new(ctx, msg.channel_id);
        loop {
            let wait = {
                let states = self.auto_state.lock().unwrap();
                states
                    .get(&channel)
                    .map_or(Duration::ZERO, |s| s.wait(next.from_bot, Instant::now()))
            };
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            // Another bot reply may have reached the limit while this one waited.
            let allowed = !next.from_bot || {
                let states = self.auto_state.lock().unwrap();
                states
                    .get(&channel)
                    .is_none_or(|s| s.bot_chain < self.profile.bot_reply_limit)
            };
            let mut answered = false;
            if allowed {
                let trigger = Trigger {
                    auto: true,
                    from_bot: next.from_bot,
                };
                let started = Instant::now();
                loop {
                    if self.stop.is_set() {
                        return;
                    }
                    let command = Command::Ask(next.text.clone());
                    match self.run(&out, command, &next.msg, trigger).await {
                        Outcome::Done => {
                            answered = true;
                            break;
                        }
                        Outcome::Busy if started.elapsed() < AUTO_WAIT => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Outcome::Busy => break,
                    }
                }
            }
            let mut states = self.auto_state.lock().unwrap();
            let state = states.entry(channel).or_default();
            if answered {
                let now = Instant::now();
                state.last_reply = Some(now);
                if next.from_bot {
                    state.last_bot_reply = Some(now);
                    state.bot_chain = state.bot_chain.saturating_add(1);
                }
            }
            match state.pending.take() {
                Some(pending) => next = pending,
                None => {
                    state.busy = false;
                    guard.done = true;
                    return;
                }
            }
        }
    }
    /// Removes the slash commands an earlier version registered (global and per server),
    /// once per bot. Returns a status note when some were removed.
    async fn remove_old_commands(&self, ctx: &Context, guilds: Vec<GuildId>) -> Option<String> {
        let marker = self.memory.dir.join(SLASH_MARKER);
        if marker.exists() {
            return None;
        }
        let (mut removed, mut failed) = (0, false);
        match AppCommand::get_global_commands(&ctx.http).await {
            Ok(list) if !list.is_empty() => {
                match AppCommand::set_global_commands(&ctx.http, vec![]).await {
                    Ok(_) => removed += list.len(),
                    Err(_) => failed = true,
                }
            }
            Ok(_) => {}
            Err(_) => failed = true,
        }
        for guild in guilds {
            match guild.get_commands(&ctx.http).await {
                Ok(list) if !list.is_empty() => match guild.set_commands(&ctx.http, vec![]).await {
                    Ok(_) => removed += list.len(),
                    Err(_) => failed = true,
                },
                Ok(_) => {}
                Err(_) => failed = true,
            }
        }
        if !failed {
            let _ = std::fs::create_dir_all(&self.memory.dir)
                .and_then(|_| std::fs::write(&marker, b"1"));
        }
        match (removed, failed) {
            (0, false) => None,
            (0, true) => Some("could not check old slash commands".into()),
            (n, _) => Some(format!("removed {n} old slash command(s)")),
        }
    }
}
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let id = ready.user.id.get();
        let previous = self.own_id.swap(id, Ordering::SeqCst);
        if previous != id {
            unregister_ids(&APP_BOTS, &[previous]);
            register_ids(&APP_BOTS, &[id]);
        }
        let auto = if self.profile.auto_channels_enabled {
            " + auto channels"
        } else {
            ""
        };
        let connected = format!(
            "Connected as {} · {} channel(s){auto} · {}",
            ready.user.name,
            self.channels.len(),
            self.characters_note()
        );
        self.set_status(connected.clone());
        let guilds = ready.guilds.iter().map(|g| g.id).collect();
        if let Some(note) = self.remove_old_commands(&ctx, guilds).await {
            self.set_status(format!("{connected} · {note}"));
        }
    }
    async fn message(&self, ctx: Context, msg: Message) {
        if self.stop.is_set() {
            return;
        }
        // Never this bot, this app's other bots, or this app's webhooks.
        let author = msg.author.id.get();
        if author == self.own_id.load(Ordering::SeqCst)
            || is_app_bot(author)
            || msg.webhook_id.is_some_and(|w| is_app_webhook(w.get()))
        {
            return;
        }
        let from_bot = msg.author.bot || msg.webhook_id.is_some();
        let guild = msg.guild_id.map(|g| g.get());
        let channel = msg.channel_id.get();
        let listed = allowed(&self.channels, &self.guilds, channel, guild);
        let auto = self.profile.auto_channels_enabled
            && self.guild_allowed(guild)
            && guild.is_some_and(|g| self.is_auto_channel(g, channel));
        // `@bot hello` does the same as `<prefix> hello`. Auto channels keep their own handling
        // of mentions (cooldowns, newest message only).
        let content = if auto {
            None
        } else {
            mention_as_prefixed(
                &msg.content,
                self.own_id.load(Ordering::SeqCst),
                &self.profile.prefix,
            )
        };
        let content = content.as_deref().unwrap_or(&msg.content);
        if !from_bot
            && let Some(command) = parse_command(content, &self.profile.prefix, self.features())
        {
            // `auto on` works in any channel of an allowed server, so admins can add one.
            let anywhere = matches!(command, Command::Auto(_)) && self.guild_allowed(guild);
            if listed || auto || anywhere {
                let out = Out::new(&ctx, msg.channel_id);
                self.run(&out, command, &msg, Trigger::default()).await;
            }
            return;
        }
        if auto {
            self.auto_message(&ctx, &msg, from_bot).await;
        }
    }
}

/// The text an auto channel answers, or None when the message is not for the bot: empty, a
/// command for another bot (`! ? . /`), or addressed to someone else by a leading mention. A
/// leading mention of this bot is removed.
fn auto_text(content: &str, own: u64) -> Option<String> {
    let mut text = content.trim();
    if text.starts_with(['!', '?', '.', '/']) {
        return None;
    }
    if let Some(rest) = text.strip_prefix("<@") {
        let rest = rest.strip_prefix('!').unwrap_or(rest);
        let end = rest.find('>')?;
        let id = rest[..end].parse::<u64>().ok()?;
        if id != own {
            return None;
        }
        text = rest[end + 1..].trim();
    }
    (!text.is_empty()).then(|| text.to_owned())
}
/// A message that starts with a mention of this bot, rewritten with the command prefix instead
/// (`<@7> play x` becomes `!ai play x`). None for anything else, or a bare mention.
fn mention_as_prefixed(content: &str, own: u64, prefix: &str) -> Option<String> {
    let rest = content.trim().strip_prefix("<@")?;
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    let end = rest.find('>')?;
    if rest[..end].parse::<u64>().ok()? != own {
        return None;
    }
    let text = rest[end + 1..].trim();
    (!text.is_empty()).then(|| format!("{prefix} {text}"))
}
/// A bot's message may get a reply only without @everyone/@here and with few mentions.
fn bot_message_ok(content: &str, mention_everyone: bool, mentions: usize) -> bool {
    !mention_everyone
        && !content.contains("@everyone")
        && !content.contains("@here")
        && mentions <= MAX_BOT_MENTIONS
}
/// The webhook ID in an official webhook URL.
fn webhook_id(url: &str) -> Option<u64> {
    let url = reqwest::Url::parse(url).ok()?;
    url.path()
        .strip_prefix("/api/webhooks/")?
        .split('/')
        .next()?
        .parse()
        .ok()
        .filter(|&id| id > 0)
}

/// Runs one engine request on a blocking thread with its own cancel flag and time limit
/// (0 = no limit), copying streamed text into `live`. Returns (streamed text, engine result,
/// whether the time limit hit).
fn run_reply(
    request: Request,
    bot_stop: Arc<StopSignal>,
    limit_secs: u64,
    live: Arc<Mutex<String>>,
) -> (String, std::result::Result<Vec<Value>, String>, bool) {
    let cancel = request.cancel.clone();
    let (tx, rx) = mpsc::channel();
    engine::spawn(request, tx);
    let deadline = (limit_secs > 0).then(|| Instant::now() + Duration::from_secs(limit_secs));
    let (mut text, mut timed_out) = (String::new(), false);
    loop {
        if bot_stop.is_set() {
            cancel.store(true, Ordering::Relaxed);
        }
        if !cancel.load(Ordering::Relaxed) && deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            cancel.store(true, Ordering::Relaxed);
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Event::Token(t)) => {
                live.lock().unwrap().push_str(&t);
                text.push_str(&t);
            }
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

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}
/// The author's display name, cleaned for use inside a prompt.
fn speaker(user: &User) -> String {
    let name = user.global_name.as_deref().unwrap_or(&user.name);
    let name: String = name.chars().filter(|c| !c.is_control()).take(32).collect();
    match name.trim() {
        "" => "User".into(),
        name => name.into(),
    }
}

/// A loaded memory file and whether it changed since it was last saved.
struct Cached {
    history: Vec<Value>,
    used: Instant,
    dirty: bool,
}

/// Saved conversation memory for one bot: data_dir()/discord/<bot>/<scope>-<id>.json.
/// Changes are kept in the cache and written by `flush` (debounced by the handler).
/// Lock order: `cache`, then `generations`.
struct Memory {
    dir: PathBuf,
    cache: Mutex<HashMap<String, Cached>>,
    /// Bumped by `clear`, so a reply that started before a reset does not restore it.
    generations: Mutex<HashMap<String, u64>>,
    flush_pending: AtomicBool,
}
impl Memory {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(HashMap::new()),
            generations: Mutex::new(HashMap::new()),
            flush_pending: AtomicBool::new(false),
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
    fn write(&self, key: &str, history: &[Value]) {
        if let Ok(bytes) = serde_json::to_vec(history) {
            let tmp = self.dir.join(format!(".{key}.json.tmp"));
            let _ = std::fs::create_dir_all(&self.dir)
                .and_then(|_| std::fs::write(&tmp, bytes))
                .and_then(|_| std::fs::rename(&tmp, self.path(key)));
        }
    }
    fn load(&self, key: &str) -> Vec<Value> {
        let mut cache = self.cache.lock().unwrap();
        if let Some(cached) = cache.get_mut(key) {
            cached.used = Instant::now();
            return cached.history.clone();
        }
        let history = self.read(key);
        self.remember(&mut cache, key, history.clone(), false);
        history
    }
    /// Adds a finished turn to the current history (not a stale copy) under the cache lock and
    /// marks it for saving. Drops the turn when the memory was cleared since generation `since`.
    fn append(&self, key: &str, turn: Vec<Value>, since: u64) {
        if turn.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().unwrap();
        if self.generation(key) != since {
            return;
        }
        let mut history = match cache.get(key) {
            Some(cached) => cached.history.clone(),
            None => self.read(key),
        };
        history.extend(turn);
        cap_history(&mut history, MAX_TURNS, MAX_BYTES);
        self.remember(&mut cache, key, history, true);
    }
    /// Writes every changed history.
    fn flush(&self) {
        self.flush_pending.store(false, Ordering::SeqCst);
        let mut cache = self.cache.lock().unwrap();
        for (key, cached) in cache.iter_mut().filter(|(_, c)| c.dirty) {
            self.write(key, &cached.history);
            cached.dirty = false;
        }
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
    /// Caches a history, evicting (and saving, if changed) the least recently used one when full.
    fn remember(
        &self,
        cache: &mut HashMap<String, Cached>,
        key: &str,
        history: Vec<Value>,
        dirty: bool,
    ) {
        if !cache.contains_key(key) && cache.len() >= CACHE_SIZE {
            let oldest = cache
                .iter()
                .min_by_key(|(_, c)| c.used)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest
                && let Some(evicted) = cache.remove(&oldest)
                && evicted.dirty
            {
                self.write(&oldest, &evicted.history);
            }
        }
        cache.insert(
            key.to_owned(),
            Cached {
                history,
                used: Instant::now(),
                dirty,
            },
        );
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
/// The command after `<prefix> `, or None when the message is not for this bot. Command words
/// are case-insensitive; music, auto and character commands exist only when `features` has
/// them on (otherwise those words are a normal question).
fn parse_command(content: &str, prefix: &str, features: Features) -> Option<Command> {
    let command = content.strip_prefix(prefix)?;
    if !command.is_empty() && !command.starts_with(char::is_whitespace) {
        return None;
    }
    let text = command.trim();
    if text.is_empty() {
        return None;
    }
    let (word, rest) = match text.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (text, ""),
    };
    let word = word.to_lowercase();
    let alone = rest.is_empty();
    let ask = || Command::Ask(text.into());
    Some(match word.as_str() {
        "stop" if alone => Command::Stop,
        "reset" if alone => Command::Reset,
        "group" => Command::Group(rest.into()),
        "play" if features.music => Command::Music(MusicCmd::Play(rest.into())),
        "skip" if features.music && alone => Command::Music(MusicCmd::Skip),
        "pause" if features.music && alone => Command::Music(MusicCmd::Pause),
        "resume" if features.music && alone => Command::Music(MusicCmd::Resume),
        "queue" if features.music && alone => Command::Music(MusicCmd::Queue),
        "leave" if features.music && alone => Command::Music(MusicCmd::Leave),
        "auto" if features.auto => match rest.to_lowercase().as_str() {
            "on" => Command::Auto(AutoCmd::On),
            "off" => Command::Auto(AutoCmd::Off),
            "" | "list" => Command::Auto(AutoCmd::List),
            _ => ask(),
        },
        "character" | "characters" if features.characters => {
            parse_character(rest).map_or_else(ask, Command::Character)
        }
        _ => ask(),
    })
}
/// `import`, `list` (or nothing), `remove <name>`, `export <name>`.
fn parse_character(rest: &str) -> Option<CharacterCmd> {
    let (word, name) = match rest.split_once(char::is_whitespace) {
        Some((word, name)) => (word, name.trim()),
        None => (rest, ""),
    };
    Some(match (word.to_lowercase().as_str(), name.is_empty()) {
        ("import", true) => CharacterCmd::Import,
        ("" | "list", true) => CharacterCmd::List,
        ("remove", false) => CharacterCmd::Remove(name.into()),
        ("export", false) => CharacterCmd::Export(name.into()),
        _ => return None,
    })
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
    // Tool steps are not remembered: a later reply may run with the tools off, and the
    // server would then get tool messages it was never offered tools for.
    turn.retain_mut(|m| {
        if m["role"] == "tool" {
            return false;
        }
        if let Some(obj) = m.as_object_mut() {
            obj.remove("tool_calls");
        }
        m["role"] != "assistant" || m["content"].as_str().is_some_and(|c| !c.trim().is_empty())
    });
    history.extend(turn);
    cap_history(history, MAX_TURNS, MAX_BYTES);
}
/// Keeps whole turns (each starting at a user message): at most `max_turns`, and drops the
/// oldest turns until the JSON fits in `max_bytes`. Each message is serialized once.
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
    // Array size = brackets + items + commas: sum(len + 1) + 1.
    let mut sizes: Vec<usize> = history
        .iter()
        .map(|m| serde_json::to_vec(m).map_or(0, |v| v.len() + 1))
        .collect();
    let mut total = sizes.iter().sum::<usize>() + 1;
    while !history.is_empty() && total > max_bytes {
        let next = history
            .iter()
            .skip(1)
            .position(|m| m["role"] == "user")
            .map_or(history.len(), |i| i + 1);
        total -= sizes[..next].iter().sum::<usize>();
        history.drain(..next);
        sizes.drain(..next);
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
/// the group (by turn order), a named character (`@Name q`, longest matching name wins), or
/// the bot's default. `personas` may include the server's own characters.
fn select_personas(
    command: &Command,
    personas: &[Persona],
    group_default: bool,
) -> (Vec<Persona>, String) {
    let usable: Vec<&Persona> = personas.iter().filter(|p| p.discord).collect();
    let group = |question: &str| -> (Vec<Persona>, String) {
        (
            group_order(usable.iter().copied())
                .into_iter()
                .cloned()
                .collect(),
            question.trim().into(),
        )
    };
    let question = match command {
        Command::Group(question) => return group(question),
        Command::Ask(question) => question.as_str(),
        Command::Stop
        | Command::Reset
        | Command::Music(_)
        | Command::Auto(_)
        | Command::Character(_) => return (vec![], String::new()),
    };
    if let Some(named) = question.strip_prefix('@') {
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
        group(question)
    } else {
        (
            usable.first().map(|p| (*p).clone()).into_iter().collect(),
            question.trim().into(),
        )
    }
}
/// The name shown above a reply: the character's, or the bot's own when answering as the
/// plain agent. At most 80 characters.
fn display_name(persona: &Persona, bot: &str) -> String {
    let name = persona.name.trim();
    let name = if name.is_empty() { bot.trim() } else { name };
    match name.chars().take(80).collect::<String>() {
        name if name.is_empty() => "Bot".into(),
        name => name,
    }
}
/// The plain agent (no character) and the question for a command, used when no character
/// applies. A group or `@Name` message is just a question here.
fn plain_selection(command: &Command) -> (Vec<Persona>, String) {
    let question = match command {
        Command::Group(question) | Command::Ask(question) => question.trim(),
        _ => return (vec![], String::new()),
    };
    (vec![Persona::default()], question.into())
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
    fn ask(text: &str) -> Command {
        Command::Ask(text.into())
    }
    #[test]
    fn group_and_named_character_routing() {
        let p = vec![persona("Reviewer", true)];
        assert_eq!(
            select_personas(&ask("@Reviewer review this"), &p, false).0[0].name,
            "Reviewer"
        );
        let group = parse_command("!ai group hello", "!ai", Features::default()).unwrap();
        assert_eq!(select_personas(&group, &p, false).0.len(), 1);
        assert!(
            select_personas(&ask("@unknown hello"), &p, false)
                .0
                .is_empty()
        );
    }
    #[test]
    fn group_follows_turn_order() {
        let mut p = vec![persona("A", true), persona("B", true), persona("C", true)];
        p[0].turn_order = 5;
        p[2].turn_order = -1;
        let (chosen, q) = select_personas(&Command::Group("hi".into()), &p, false);
        let names: Vec<&str> = chosen.iter().map(|p| p.name.as_str()).collect();
        assert_eq!((names, q.as_str()), (vec!["C", "B", "A"], "hi"));
    }
    #[test]
    fn routing_matches_longest_unicode_multi_word_name() {
        let p = vec![
            persona("Code", true),
            persona("Code Reviewer", true),
            persona("Élodie", true),
        ];
        let (chosen, question) = select_personas(&ask("@code reviewer  check this"), &p, false);
        assert_eq!(
            (chosen[0].name.as_str(), question.as_str()),
            ("Code Reviewer", "check this")
        );
        assert_eq!(
            select_personas(&ask("@Code hi"), &p, false).0[0].name,
            "Code"
        );
        assert_eq!(
            select_personas(&ask("@ÉLODIE bonjour"), &p, false).0[0].name,
            "Élodie"
        );
        assert!(select_personas(&ask("@Codex hi"), &p, false).0.is_empty());
        assert!(select_personas(&ask("@Code"), &p, false).1.is_empty());
    }
    #[test]
    fn only_discord_characters_are_used() {
        let p = vec![persona("Private", false), persona("Public", true)];
        assert_eq!(
            select_personas(&ask("hello"), &p, false).0[0].name,
            "Public"
        );
        assert!(select_personas(&ask("@Private hi"), &p, false).0.is_empty());
        assert_eq!(select_personas(&ask("hi"), &p, true).0.len(), 1);
        assert!(
            select_personas(&ask("hi"), &[persona("Private", false)], false)
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
        assert_eq!(token_key("My Bot"), "discord-token-my_bot");
        assert_eq!(webhooks_key("My Bot"), "discord-webhooks-my_bot");
    }
    #[test]
    fn commands_ids_and_allowlists() {
        let parse = |text: &str| parse_command(text, "!ai", Features::default());
        assert_eq!(parse("!ai  stop "), Some(Command::Stop));
        assert_eq!(parse("!ai RESET"), Some(Command::Reset));
        assert_eq!(
            parse("!ai Group  plan it"),
            Some(Command::Group("plan it".into()))
        );
        assert_eq!(parse("!ai group"), Some(Command::Group("".into())));
        assert_eq!(parse("!ai groupie hi"), Some(ask("groupie hi")));
        assert_eq!(parse("!ai @Ada hi"), Some(ask("@Ada hi")));
        assert_eq!(parse("!aihello"), None);
        assert_eq!(parse("!ai"), None);
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
    fn optional_commands_need_their_feature() {
        let all = Features {
            music: true,
            auto: true,
            characters: true,
        };
        let parse = |text: &str| parse_command(text, "!ai", all);
        assert_eq!(
            parse("!ai PLAY  never gonna "),
            Some(Command::Music(MusicCmd::Play("never gonna".into())))
        );
        assert_eq!(
            parse("!ai play"),
            Some(Command::Music(MusicCmd::Play("".into())))
        );
        assert_eq!(parse("!ai skip"), Some(Command::Music(MusicCmd::Skip)));
        assert_eq!(parse("!ai Queue"), Some(Command::Music(MusicCmd::Queue)));
        assert_eq!(parse("!ai leave"), Some(Command::Music(MusicCmd::Leave)));
        // Extra words make it a question.
        assert_eq!(parse("!ai skip this part"), Some(ask("skip this part")));
        assert_eq!(parse("!ai stop it"), Some(ask("stop it")));
        assert_eq!(parse("!ai auto ON"), Some(Command::Auto(AutoCmd::On)));
        assert_eq!(parse("!ai auto off"), Some(Command::Auto(AutoCmd::Off)));
        assert_eq!(parse("!ai auto"), Some(Command::Auto(AutoCmd::List)));
        assert_eq!(parse("!ai auto pilot"), Some(ask("auto pilot")));
        assert_eq!(
            parse("!ai character import"),
            Some(Command::Character(CharacterCmd::Import))
        );
        assert_eq!(
            parse("!ai characters"),
            Some(Command::Character(CharacterCmd::List))
        );
        assert_eq!(
            parse("!ai character remove  Ada Lovelace "),
            Some(Command::Character(CharacterCmd::Remove(
                "Ada Lovelace".into()
            )))
        );
        assert_eq!(
            parse("!ai character Export Ada"),
            Some(Command::Character(CharacterCmd::Export("Ada".into())))
        );
        assert_eq!(parse("!ai character remove"), Some(ask("character remove")));
        assert_eq!(parse("!ai character design"), Some(ask("character design")));
        let none = Features::default();
        assert_eq!(
            parse_command("!ai play x", "!ai", none),
            Some(ask("play x"))
        );
        assert_eq!(parse_command("!ai skip", "!ai", none), Some(ask("skip")));
        assert_eq!(
            parse_command("!ai auto on", "!ai", none),
            Some(ask("auto on"))
        );
        assert_eq!(
            parse_command("!ai character list", "!ai", none),
            Some(ask("character list"))
        );
        // Commands are not routed to characters.
        let p = vec![persona("Ada", true)];
        assert!(
            select_personas(&Command::Music(MusicCmd::Skip), &p, true)
                .0
                .is_empty()
        );
    }
    #[test]
    fn auto_channel_text_filters() {
        assert_eq!(
            auto_text("  hello there ", 7).as_deref(),
            Some("hello there")
        );
        for skipped in ["!play", "?help", ".ping", "/roll", "   ", ""] {
            assert_eq!(auto_text(skipped, 7), None, "{skipped:?}");
        }
        // Addressed to this bot: the mention is removed. To someone else: skipped.
        assert_eq!(
            mention_as_prefixed("<@7> hello", 7, "!ai").as_deref(),
            Some("!ai hello")
        );
        assert_eq!(
            mention_as_prefixed("<@!7> play x", 7, "!ai").as_deref(),
            Some("!ai play x")
        );
        assert_eq!(mention_as_prefixed("<@8> hello", 7, "!ai"), None);
        assert_eq!(mention_as_prefixed("<@7>", 7, "!ai"), None);
        assert_eq!(mention_as_prefixed("hi <@7>", 7, "!ai"), None);
        assert_eq!(auto_text("<@7> hi", 7).as_deref(), Some("hi"));
        assert_eq!(auto_text("<@!7> hi", 7).as_deref(), Some("hi"));
        assert_eq!(auto_text("<@8> hi", 7), None);
        assert_eq!(auto_text("<@&9> hi", 7), None);
        assert_eq!(auto_text("<@7>", 7), None);
        assert_eq!(auto_text("hi <@8>", 7).as_deref(), Some("hi <@8>"));
    }
    #[test]
    fn bot_messages_are_guarded() {
        assert!(bot_message_ok("hello", false, 0));
        assert!(bot_message_ok("hi <@1>", false, MAX_BOT_MENTIONS));
        assert!(!bot_message_ok("hi", false, MAX_BOT_MENTIONS + 1));
        assert!(!bot_message_ok("hi", true, 0));
        assert!(!bot_message_ok("ping @everyone", false, 0));
        assert!(!bot_message_ok("ping @here", false, 0));
    }
    #[test]
    fn auto_cooldowns() {
        let now = Instant::now();
        let mut state = AutoState::default();
        assert_eq!(state.wait(true, now), Duration::ZERO);
        state.last_reply = Some(now);
        assert_eq!(state.wait(false, now), AUTO_CHANNEL_COOLDOWN);
        state.last_bot_reply = Some(now);
        assert_eq!(state.wait(true, now), BOT_COOLDOWN);
        let later = now + Duration::from_secs(4);
        assert_eq!(state.wait(false, later), Duration::ZERO);
        assert_eq!(state.wait(true, later), Duration::from_secs(6));
        assert_eq!(
            state.wait(true, now + Duration::from_secs(60)),
            Duration::ZERO
        );
    }
    #[test]
    fn app_registry_and_webhook_ids() {
        assert_eq!(
            webhook_id("https://discord.com/api/webhooks/123/abc"),
            Some(123)
        );
        assert_eq!(webhook_id("https://discord.com/api/webhooks/x/abc"), None);
        assert_eq!(webhook_id("not a url"), None);
        // IDs no other test uses; one ID may be listed by two bots at once.
        let (bot, hook) = (9_000_000_001, 9_000_000_002);
        register_ids(&APP_BOTS, &[bot, 0]);
        register_ids(&APP_BOTS, &[bot]);
        register_ids(&APP_WEBHOOKS, &[hook]);
        assert!(is_app_bot(bot) && is_app_webhook(hook) && !is_app_bot(0));
        unregister_ids(&APP_BOTS, &[bot]);
        assert!(is_app_bot(bot));
        unregister_ids(&APP_BOTS, &[bot]);
        unregister_ids(&APP_WEBHOOKS, &[hook]);
        assert!(!is_app_bot(bot) && !is_app_webhook(hook));
    }
    #[test]
    fn new_profile_fields_default_off() {
        let old: BotProfile = serde_json::from_str(r#"{"name":"Old","prefix":"!x"}"#).unwrap();
        assert!(!old.tools.any() && !old.auto_channels_enabled && !old.bot_replies);
        assert!(!old.server_characters);
        assert_eq!(
            (
                old.bot_reply_limit,
                old.server_char_limit,
                old.server_char_max_kb
            ),
            (3, 10, 32)
        );
        assert_eq!(
            (
                old.tools.music_tools.max_queue_per_user,
                old.tools.music_tools.max_track_minutes
            ),
            (5, 15)
        );
        let partial: BotTools = serde_json::from_str(r#"{"music":true,"music_tools":{}}"#).unwrap();
        assert!(partial.music && partial.any());
        assert_eq!(partial.music_tools.max_queue_per_user, 5);
        assert!(!partial.moderation_settings.allow_ban);
        assert!(intents(false, false).contains(GatewayIntents::MESSAGE_CONTENT));
        assert!(!intents(false, false).contains(GatewayIntents::GUILD_VOICE_STATES));
        assert!(intents(true, false).contains(GatewayIntents::GUILD_VOICE_STATES));
        assert!(intents(false, true).contains(GatewayIntents::GUILD_MESSAGE_REACTIONS));
    }
    #[test]
    fn admin_permissions() {
        assert!(has_permission(
            Permissions::ADMINISTRATOR,
            Permissions::MANAGE_GUILD
        ));
        assert!(has_permission(
            Permissions::MANAGE_CHANNELS | Permissions::SEND_MESSAGES,
            Permissions::MANAGE_CHANNELS
        ));
        assert!(!has_permission(
            Permissions::SEND_MESSAGES,
            Permissions::MANAGE_CHANNELS
        ));
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
        // Writes wait for a flush, then reach disk.
        assert!(!memory.path("k").exists());
        memory.flush();
        assert_eq!(Memory::new(dir.clone()).load("k").len(), 4);
        // A reply that started before a reset does not restore the old history.
        let stale = memory.generation("k");
        memory.clear("k");
        memory.append("k", turn("late"), stale);
        memory.flush();
        assert!(memory.load("k").is_empty());
        assert!(Memory::new(dir.clone()).load("k").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn evicted_changes_are_saved() {
        let dir = std::env::temp_dir().join(format!("kyttu-evict-{}", std::process::id()));
        let memory = Memory::new(dir.clone());
        let turn = vec![
            json!({"role":"user","content":"q"}),
            json!({"role":"assistant","content":"a"}),
        ];
        for i in 0..=CACHE_SIZE {
            memory.append(&format!("k{i}"), turn.clone(), 0);
        }
        // The one evicted history was written; the cached ones wait for a flush.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
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
        let size = serde_json::to_vec(&long).unwrap().len();
        assert!(size <= 200 && size > 100 && long[0]["role"] == "user");
    }
    #[test]
    fn discord_chunks_respect_utf16_limits() {
        let text = "🦊".repeat(2000);
        let chunks = message_chunks(&text);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|s| s.encode_utf16().count() <= 1700));
        // Streaming relies on earlier chunks staying the same as text grows.
        let longer = format!("{text}more");
        assert_eq!(
            message_chunks(&longer)[..chunks.len() - 1],
            chunks[..chunks.len() - 1]
        );
    }
}

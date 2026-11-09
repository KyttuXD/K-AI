use crate::{
    config::{Config, data_dir},
    core::personas::{Persona, group_order},
    docs::Library,
    engine::{self, Event, Request},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serenity::{
    all::{
        ChannelId, CommandInteraction, CommandOptionType, Context, CreateAllowedMentions,
        CreateCommand, CreateCommandOption, CreateInteractionResponse,
        CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, EditMessage,
        EventHandler, ExecuteWebhook, GatewayIntents, GuildId, Interaction, Message, MessageId,
        Ready, User, Webhook,
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
/// How often a streaming reply message is edited.
const EDIT_INTERVAL: Duration = Duration::from_millis(1500);
/// Changed memory is saved this long after the first change (several replies share a write).
const FLUSH_DELAY: Duration = Duration::from_secs(2);
const POST_FAILED: &str = "Could not post. Check bot/webhook permissions and rate limits.";

/// Which conversation a bot remembers: per channel, per server or per user.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum MemoryScope {
    #[default]
    Channel,
    Server,
    User,
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

/// One bot session. Several can run at once; they share no global state.
pub struct Discord {
    pub status: Arc<Mutex<String>>,
    pub running: Arc<AtomicBool>,
    stop: Mutex<Arc<StopSignal>>,
    replies: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
}
impl Default for Discord {
    fn default() -> Self {
        Self {
            status: Arc::new(Mutex::new("Disconnected".into())),
            running: Arc::new(AtomicBool::new(false)),
            stop: Mutex::new(Arc::default()),
            replies: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
impl Discord {
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
        let stop = Arc::new(StopSignal::default());
        *self.stop.lock().unwrap() = stop.clone();
        self.replies.lock().unwrap().clear();
        let running = self.running.clone();
        let status = self.status.clone();
        *status.lock().unwrap() = "Connecting…".into();
        let memory = Arc::new(Memory::new(
            data_dir().join("discord").join(sanitize(&profile.name)),
        ));
        let handler = Handler {
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
            memory.flush();
            running.store(false, Ordering::Relaxed);
        });
        Ok(())
    }
}

async fn run_client(
    token: String,
    handler: Handler,
    status: Arc<Mutex<String>>,
    stop: Arc<StopSignal>,
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
    if stop.is_set() {
        *status.lock().unwrap() = "Disconnected".into();
        return;
    }
    let manager = client.shard_manager.clone();
    let stopping = stop.clone();
    // notify_one keeps a permit, so a stop that happened before this waits is not missed.
    let watcher = tokio::spawn(async move {
        stopping.wake.notified().await;
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

/// What a user asked for, from a prefixed message or a slash command.
#[derive(Debug, PartialEq)]
enum Command {
    Stop,
    Reset,
    /// Every group character answers.
    Group(String),
    /// The default character(s), or `@Name question`.
    Ask(String),
    /// A named character (slash command option).
    AskAs(String, String),
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

/// A message this reply posted: the slash command's own response, or a channel message.
enum Posted {
    Original,
    Channel(MessageId),
}

/// Where one request's output goes. A slash command gets an ephemeral answer for quick
/// notices, or a deferred response that the first streamed message fills.
struct Out<'a> {
    ctx: &'a Context,
    channel: ChannelId,
    slash: Option<&'a CommandInteraction>,
    /// The interaction already has a response (deferred or ephemeral).
    responded: bool,
    /// The deferred response still waits for content.
    original_free: bool,
}
// serenity::Error is serenity's own type; boxing it here would only move the allocation.
#[allow(clippy::result_large_err)]
impl<'a> Out<'a> {
    fn new(ctx: &'a Context, channel: ChannelId, slash: Option<&'a CommandInteraction>) -> Self {
        Self {
            ctx,
            channel,
            slash,
            responded: false,
            original_free: false,
        }
    }
    /// A short notice: ephemeral for an unanswered slash command, else a channel message.
    async fn say(&mut self, text: &str) {
        if let Some(cmd) = self.slash
            && !self.responded
        {
            self.responded = true;
            let message = CreateInteractionResponseMessage::new()
                .content(text)
                .ephemeral(true)
                .allowed_mentions(no_mentions());
            let response = CreateInteractionResponse::Message(message);
            let _ = cmd.create_response(&self.ctx.http, response).await;
            return;
        }
        let _ = self.post(text.to_owned()).await;
    }
    /// Shows the slash command as "thinking" while the reply is generated.
    async fn defer(&mut self) -> serenity::Result<()> {
        if let Some(cmd) = self.slash
            && !self.responded
        {
            let response =
                CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new());
            cmd.create_response(&self.ctx.http, response).await?;
            self.responded = true;
            self.original_free = true;
        }
        Ok(())
    }
    async fn post(&mut self, content: String) -> serenity::Result<Posted> {
        if let Some(cmd) = self.slash
            && self.original_free
        {
            self.original_free = false;
            let edit = EditInteractionResponse::new()
                .content(content)
                .allowed_mentions(no_mentions());
            cmd.edit_response(&self.ctx.http, edit).await?;
            return Ok(Posted::Original);
        }
        let message = CreateMessage::new()
            .content(content)
            .allowed_mentions(no_mentions());
        let sent = self.channel.send_message(&self.ctx.http, message).await?;
        Ok(Posted::Channel(sent.id))
    }
    async fn edit(&self, posted: &Posted, content: String) -> serenity::Result<()> {
        match (posted, self.slash) {
            (Posted::Original, Some(cmd)) => {
                let edit = EditInteractionResponse::new()
                    .content(content)
                    .allowed_mentions(no_mentions());
                cmd.edit_response(&self.ctx.http, edit).await.map(|_| ())
            }
            (Posted::Original, None) => Ok(()),
            (Posted::Channel(id), _) => {
                let edit = EditMessage::new()
                    .content(content)
                    .allowed_mentions(no_mentions());
                self.channel
                    .edit_message(&self.ctx.http, *id, edit)
                    .await
                    .map(|_| ())
            }
        }
    }
    /// Removes an unused "thinking" response (for example when webhooks posted the answer).
    async fn finish(&mut self) {
        if let Some(cmd) = self.slash
            && self.original_free
        {
            self.original_free = false;
            let _ = cmd.delete_response(&self.ctx.http).await;
        }
    }
}

/// Posts or edits the messages of one streamed answer so they show `text`; `stream` holds the
/// messages already posted and what each shows. Earlier chunks stop changing once full.
#[allow(clippy::result_large_err)]
async fn show(
    out: &mut Out<'_>,
    stream: &mut Vec<(Posted, String)>,
    name: &str,
    text: &str,
) -> serenity::Result<()> {
    for (i, chunk) in message_chunks(text).into_iter().enumerate() {
        let content = format!("**{name}**\n{chunk}");
        match stream.get_mut(i) {
            Some((_, shown)) if *shown == content => {}
            Some((posted, shown)) => {
                out.edit(posted, content.clone()).await?;
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
    /// Runs one command for `author` in an allowed channel (checked by the caller).
    async fn run(&self, out: &mut Out<'_>, command: Command, guild: Option<u64>, author: &User) {
        let channel = out.channel.get();
        let user = author.id.get();
        let slash = out.slash.is_some();
        if !self.cooldown_ok(user) {
            if slash {
                out.say("Please wait a few seconds.").await;
            }
            return;
        }
        let key = scope_key(&self.profile.memory, channel, guild, user);
        match command {
            Command::Stop => {
                let flag = self.replies.lock().unwrap().get(&channel).cloned();
                let text = match flag {
                    Some(flag) => {
                        flag.store(true, Ordering::Relaxed);
                        "Stopping the current reply."
                    }
                    None => "Nothing is running in this channel.",
                };
                out.say(text).await;
                return;
            }
            Command::Reset => {
                self.memory.clear(&key);
                out.say("Memory cleared.").await;
                return;
            }
            _ => {}
        }
        if !self.config.personas.iter().any(|p| p.discord) {
            out.say("No characters are available on Discord yet. Turn on \"Available on Discord\" for a character in KYTTU - AI.").await;
            return;
        }
        let (selected, question) =
            select_personas(&command, &self.config.personas, self.profile.group);
        if question.is_empty() {
            if slash {
                out.say("Add a question.").await;
            }
            return;
        }
        if selected.is_empty() {
            out.say("No Discord character has that name.").await;
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
            out.say("Busy with another reply. Try again shortly.").await;
            return;
        }
        let _active = ActiveReply {
            replies: self.replies.clone(),
            channel,
        };
        if out.defer().await.is_err() {
            return;
        }
        let webhook = match self.webhooks.get(&channel) {
            None => None,
            Some(url) => match Webhook::from_url(&out.ctx.http, url).await {
                Ok(hook) if hook.channel_id == Some(out.channel) => Some(hook),
                _ => {
                    self.set_status("Webhook validation failed or it belongs to a different channel. Nothing was posted.");
                    out.say("This channel's webhook is not usable.").await;
                    return;
                }
            },
        };
        let speaker_line = format!("{}: {question}", speaker(author));
        // Read the generation first so a reset during this reply drops its turn.
        let since = self.memory.generation(&key);
        let history = self.memory.load(&key);
        let mut new_turn = vec![];
        let mut failed = false;
        for (index, persona) in selected.iter().enumerate() {
            if self.stop.is_set() || cancel.load(Ordering::Relaxed) {
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
            let name = persona.name.chars().take(80).collect::<String>();
            let (bot_stop, limit) = (self.stop.clone(), self.profile.reply_timeout_secs);
            let live = Arc::new(Mutex::new(String::new()));
            let worker_live = live.clone();
            let mut job = tokio::task::spawn_blocking(move || {
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
                let text = format!(
                    "{} hit the {limit}s time limit and was stopped.",
                    persona.name
                );
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
        out.finish().await;
        if !failed && !self.stop.is_set() {
            self.set_status("Connected · waiting for a message");
        }
    }
}
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        // Slash commands go to each allowed server (or every server the bot is in); guild
        // commands update instantly, unlike global ones.
        let guilds: Vec<GuildId> = if self.guilds.is_empty() {
            ready.guilds.iter().map(|g| g.id).collect()
        } else {
            self.guilds.iter().map(|&g| GuildId::new(g)).collect()
        };
        let mut failed = 0;
        for guild in guilds {
            if guild
                .set_commands(&ctx.http, slash_commands())
                .await
                .is_err()
            {
                failed += 1;
            }
        }
        let note = if failed > 0 {
            format!(" · slash commands failed in {failed} server(s)")
        } else {
            String::new()
        };
        self.set_status(format!(
            "Connected as {} · {} channel(s){note}",
            ready.user.name,
            self.channels.len()
        ));
    }
    async fn message(&self, ctx: Context, msg: Message) {
        if self.stop.is_set() || msg.author.bot || msg.webhook_id.is_some() {
            return;
        }
        let guild = msg.guild_id.map(|g| g.get());
        if !allowed(&self.channels, &self.guilds, msg.channel_id.get(), guild) {
            return;
        }
        let Some(command) = parse_command(&msg.content, &self.profile.prefix) else {
            return;
        };
        let mut out = Out::new(&ctx, msg.channel_id, None);
        self.run(&mut out, command, guild, &msg.author).await;
    }
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Command(cmd) = interaction else {
            return;
        };
        if self.stop.is_set() || cmd.user.bot {
            return;
        }
        let mut out = Out::new(&ctx, cmd.channel_id, Some(&cmd));
        let guild = cmd.guild_id.map(|g| g.get());
        if !allowed(&self.channels, &self.guilds, cmd.channel_id.get(), guild) {
            out.say("This bot does not answer in this channel.").await;
            return;
        }
        let command = parse_slash(
            &cmd.data.name,
            option(&cmd, "question"),
            option(&cmd, "character"),
        );
        match command {
            Some(command) => self.run(&mut out, command, guild, &cmd.user).await,
            None => out.say("Unknown command.").await,
        }
    }
}

fn option<'a>(cmd: &'a CommandInteraction, name: &str) -> Option<&'a str> {
    cmd.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| o.value.as_str())
}

/// /ask, /group, /stop and /reset.
fn slash_commands() -> Vec<CreateCommand> {
    let question = || {
        CreateCommandOption::new(CommandOptionType::String, "question", "What to ask")
            .required(true)
            .max_length(2000)
    };
    vec![
        CreateCommand::new("ask")
            .description("Ask a character")
            .add_option(question())
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "character",
                    "Character name (optional)",
                )
                .max_length(64),
            ),
        CreateCommand::new("group")
            .description("Ask every group character")
            .add_option(question()),
        CreateCommand::new("stop").description("Stop the current reply"),
        CreateCommand::new("reset").description("Clear this conversation's memory"),
    ]
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
/// The command after `<prefix> `, or None when the message is not for this bot.
fn parse_command(content: &str, prefix: &str) -> Option<Command> {
    let command = content.strip_prefix(prefix)?;
    if !command.is_empty() && !command.starts_with(char::is_whitespace) {
        return None;
    }
    Some(match command.trim() {
        "" => return None,
        c if c.eq_ignore_ascii_case("stop") => Command::Stop,
        c if c.eq_ignore_ascii_case("reset") => Command::Reset,
        c => match strip_word_ci(c, "group") {
            Some(question) => Command::Group(question.trim().into()),
            None => Command::Ask(c.into()),
        },
    })
}
/// A slash command and its options.
fn parse_slash(name: &str, question: Option<&str>, character: Option<&str>) -> Option<Command> {
    let question = question.unwrap_or_default().trim().to_owned();
    Some(match name {
        "stop" => Command::Stop,
        "reset" => Command::Reset,
        "group" => Command::Group(question),
        "ask" => match character.map(str::trim).filter(|c| !c.is_empty()) {
            Some(character) => Command::AskAs(character.into(), question),
            None => Command::Ask(question),
        },
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
/// the group (by turn order), a named character (`@Name q`, longest matching name wins, or the
/// slash option), or the bot's default.
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
        Command::Stop | Command::Reset => return (vec![], String::new()),
        Command::Group(question) => return group(question),
        Command::AskAs(name, question) => {
            let name = name.trim().to_lowercase();
            let chosen = usable.iter().find(|p| p.name.trim().to_lowercase() == name);
            return (
                chosen.map(|p| (*p).clone()).into_iter().collect(),
                question.trim().into(),
            );
        }
        Command::Ask(question) => question.as_str(),
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
        let group = parse_command("!ai group hello", "!ai").unwrap();
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
        let named = Command::AskAs(" code reviewer ".into(), "hi".into());
        assert_eq!(
            select_personas(&named, &p, false).0[0].name,
            "Code Reviewer"
        );
        let unknown = Command::AskAs("Codex".into(), "hi".into());
        assert!(select_personas(&unknown, &p, false).0.is_empty());
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
        let private = Command::AskAs("Private".into(), "hi".into());
        assert!(select_personas(&private, &p, false).0.is_empty());
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
        assert_eq!(parse_command("!ai  stop ", "!ai"), Some(Command::Stop));
        assert_eq!(parse_command("!ai RESET", "!ai"), Some(Command::Reset));
        assert_eq!(
            parse_command("!ai Group  plan it", "!ai"),
            Some(Command::Group("plan it".into()))
        );
        assert_eq!(
            parse_command("!ai groupie hi", "!ai"),
            Some(ask("groupie hi"))
        );
        assert_eq!(parse_command("!ai @Ada hi", "!ai"), Some(ask("@Ada hi")));
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
    fn slash_commands_parse() {
        assert_eq!(parse_slash("stop", None, None), Some(Command::Stop));
        assert_eq!(parse_slash("reset", None, None), Some(Command::Reset));
        assert_eq!(
            parse_slash("group", Some(" ideas "), None),
            Some(Command::Group("ideas".into()))
        );
        // A question that looks like a text command is still a question.
        assert_eq!(
            parse_slash("ask", Some("stop"), Some("  ")),
            Some(ask("stop"))
        );
        assert_eq!(
            parse_slash("ask", Some("hi"), Some(" Ada ")),
            Some(Command::AskAs("Ada".into(), "hi".into()))
        );
        assert_eq!(parse_slash("ask", None, None), Some(ask("")));
        assert_eq!(parse_slash("nope", Some("x"), None), None);
        let names: Vec<String> = slash_commands()
            .iter()
            .map(|c| {
                serde_json::to_value(c).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(names, ["ask", "group", "stop", "reset"]);
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

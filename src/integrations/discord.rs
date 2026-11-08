use crate::{
    config::Config,
    core::personas::Persona,
    docs::Library,
    engine::{self, Event, Request},
};
use anyhow::{Result, ensure};
use serde_json::Value;
use serenity::{
    all::{
        ChannelId, Context, CreateAllowedMentions, CreateMessage, EventHandler, ExecuteWebhook,
        GatewayIntents, Message, Ready, Webhook,
    },
    async_trait,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

pub struct Discord {
    pub status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    pub running: Arc<AtomicBool>,
}
impl Default for Discord {
    fn default() -> Self {
        Self {
            status: Arc::new(Mutex::new("Disconnected".into())),
            stop: Arc::new(AtomicBool::new(false)),
            running: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Discord {
    pub fn disconnect(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn connect(
        &mut self,
        token: String,
        webhook: String,
        config: Config,
        library: Library,
    ) -> Result<()> {
        ensure!(
            !self.running.load(Ordering::Relaxed),
            "Disconnect the current bot first"
        );
        ensure!(!token.trim().is_empty(), "Enter your Discord bot token");
        let channel: u64 = config
            .discord_channel
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Enter a numeric Discord channel ID"))?;
        ensure!(channel > 0, "A channel ID is required");
        ensure!(
            !config.discord_prefix.trim().is_empty(),
            "A command prefix is required"
        );
        ensure!(!config.personas.is_empty(), "Create a character first");
        if !webhook.trim().is_empty() {
            let url = reqwest::Url::parse(webhook.trim())
                .map_err(|_| anyhow::anyhow!("Invalid webhook URL"))?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str() == Some("discord.com")
                    && url.path().starts_with("/api/webhooks/"),
                "Use an official https://discord.com/api/webhooks/… URL"
            );
        }
        self.stop = Arc::new(AtomicBool::new(false));
        let stop = self.stop.clone();
        let running = self.running.clone();
        let status = self.status.clone();
        running.store(true, Ordering::Relaxed);
        *status.lock().unwrap() = "Connecting…".into();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build();
            if let Ok(runtime) = result {
                runtime.block_on(async {
                    let handler = Handler { config, library, channel, webhook_url: webhook, status: status.clone(), stop: stop.clone(), busy: Arc::new(tokio::sync::Mutex::new(())), histories: Mutex::new(HashMap::new()) };
                    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
                    match serenity::Client::builder(token.trim(),intents).event_handler(handler).await {
                        Ok(mut client) => {
                            if stop.load(Ordering::Relaxed) { *status.lock().unwrap() = "Disconnected".into(); return; }
                            let manager = client.shard_manager.clone(); let stopping = stop.clone();
                            let watcher = tokio::spawn(async move {
                                while !stopping.load(Ordering::Relaxed) { tokio::time::sleep(Duration::from_millis(250)).await; }
                                manager.shutdown_all().await;
                            });
                            if client.start().await.is_err() { *status.lock().unwrap() = "Connection failed. Check token, Message Content Intent, channel access, and network.".into(); }
                            else { *status.lock().unwrap() = "Disconnected".into(); }
                            stop.store(true,Ordering::Relaxed); watcher.abort();
                        }
                        Err(_) => { *status.lock().unwrap() = "Could not connect. Check your bot token and network.".into(); }
                    }
                });
            } else {
                *status.lock().unwrap() = "Could not initialize Discord runtime".into();
            }
            running.store(false, Ordering::Relaxed);
        });
        Ok(())
    }
}
type Histories = HashMap<u64, Vec<Value>>;
struct Handler {
    config: Config,
    library: Library,
    channel: u64,
    webhook_url: String,
    status: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    busy: Arc<tokio::sync::Mutex<()>>,
    histories: Mutex<Histories>,
}
#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        *self.status.lock().unwrap() = format!(
            "Connected as {} · channel {}",
            ready.user.name, self.channel
        );
    }
    async fn message(&self, ctx: Context, msg: Message) {
        if self.stop.load(Ordering::Relaxed)
            || msg.author.bot
            || msg.webhook_id.is_some()
            || msg.channel_id.get() != self.channel
        {
            return;
        }
        let Some(command) = msg.content.strip_prefix(&self.config.discord_prefix) else {
            return;
        };
        if !command.is_empty() && !command.starts_with(char::is_whitespace) {
            return;
        }
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        let Ok(_guard) = self.busy.try_lock() else {
            return;
        }; // One local inference at a time; no unbounded queue.
        let (selected, question) =
            select_personas(command, &self.config.personas, self.config.discord_group);
        if question.is_empty() || selected.is_empty() {
            return;
        }
        let mut history = self
            .histories
            .lock()
            .unwrap()
            .get(&msg.author.id.get())
            .cloned()
            .unwrap_or_default();
        let webhook = if self.webhook_url.is_empty() {
            None
        } else {
            match Webhook::from_url(&ctx.http, &self.webhook_url).await {
                Ok(hook) if hook.channel_id == Some(ChannelId::new(self.channel)) => Some(hook),
                _ => {
                    *self.status.lock().unwrap() = "Webhook validation failed or it belongs to a different channel. Nothing was posted.".into();
                    return;
                }
            }
        };
        for persona in selected {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let config = self.config.clone();
            let library = self.library.clone();
            let question = question.clone();
            let prior = history.clone();
            let character = persona.clone();
            let stop = self.stop.clone();
            *self.status.lock().unwrap() = format!("{} is responding…", persona.name);
            let generated = tokio::task::spawn_blocking(move || {
                let (tx, rx) = mpsc::channel();
                let mut cfg = config;
                if !character.model.is_empty() {
                    cfg.model = character.model.clone();
                }
                let use_docs = cfg.discord_docs;
                engine::spawn(
                    Request {
                        config: cfg,
                        library,
                        question,
                        history: prior,
                        coding: false,
                        use_docs,
                        pack: String::new(),
                        cancel: stop,
                        persona: character,
                        use_web: false,
                        use_embeddings: false,
                    },
                    tx,
                );
                let mut text = String::new();
                let history = loop {
                    match rx.recv() {
                        Ok(Event::Token(t)) => text.push_str(&t),
                        Ok(Event::Approval(a)) => {
                            let _ = a.reply.send(false);
                        }
                        Ok(Event::Finished(result)) => {
                            break result?;
                        }
                        Err(_) => return Err("Model worker disconnected".to_owned()),
                        _ => {}
                    }
                };
                Ok::<_, String>((text, history))
            })
            .await;
            let Ok(Ok((text, next_history))) = generated else {
                *self.status.lock().unwrap() =
                    "Local model request failed. Check Ollama and documentation settings.".into();
                break;
            };
            history = next_history;
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            for chunk in message_chunks(&text) {
                let mentions = CreateAllowedMentions::new()
                    .all_users(false)
                    .all_roles(false)
                    .everyone(false)
                    .replied_user(false);
                let result = if let Some(hook) = &webhook {
                    let mut builder = ExecuteWebhook::new()
                        .content(chunk)
                        .username(persona.name.chars().take(80).collect::<String>())
                        .allowed_mentions(mentions);
                    if persona.avatar_url.starts_with("https://") {
                        builder = builder.avatar_url(&persona.avatar_url);
                    }
                    hook.execute(&ctx.http, false, builder).await.map(|_| ())
                } else {
                    msg.channel_id
                        .send_message(
                            &ctx.http,
                            CreateMessage::new()
                                .content(format!(
                                    "**{}**\n{chunk}",
                                    persona.name.chars().take(80).collect::<String>()
                                ))
                                .allowed_mentions(mentions),
                        )
                        .await
                        .map(|_| ())
                };
                if result.is_err() {
                    *self.status.lock().unwrap() =
                        "Could not post. Check bot/webhook permissions and rate limits.".into();
                    return;
                }
            }
        }
        let mut histories = self.histories.lock().unwrap();
        if histories.len() >= 50 {
            histories.clear();
        }
        histories.insert(msg.author.id.get(), history);
        if !self.stop.load(Ordering::Relaxed) {
            *self.status.lock().unwrap() = "Connected · waiting for a prefixed message".into();
        }
    }
}

fn select_personas(
    command: &str,
    personas: &[Persona],
    group_default: bool,
) -> (Vec<Persona>, String) {
    if let Some(question) = command.strip_prefix("group ") {
        return (
            personas
                .iter()
                .filter(|p| p.in_group)
                .take(6)
                .cloned()
                .collect(),
            question.trim().into(),
        );
    }
    if let Some(named) = command.strip_prefix('@') {
        if let Some((name, question)) = named.split_once(' ') {
            return (
                personas
                    .iter()
                    .filter(|p| p.name.eq_ignore_ascii_case(name))
                    .take(1)
                    .cloned()
                    .collect(),
                question.trim().into(),
            );
        }
        return (vec![], String::new());
    }
    if group_default {
        (
            personas
                .iter()
                .filter(|p| p.in_group)
                .take(6)
                .cloned()
                .collect(),
            command.into(),
        )
    } else {
        (
            personas.first().cloned().into_iter().collect(),
            command.into(),
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
    #[test]
    fn group_and_named_character_routing() {
        let p = vec![Persona {
            name: "Reviewer".into(),
            ..Default::default()
        }];
        assert_eq!(
            select_personas("@Reviewer review this", &p, false).0[0].name,
            "Reviewer"
        );
        assert_eq!(select_personas("group hello", &p, false).0.len(), 1);
        assert!(select_personas("@unknown hello", &p, false).0.is_empty());
    }
    #[test]
    fn discord_chunks_respect_utf16_limits() {
        let text = "🦊".repeat(2000);
        let chunks = message_chunks(&text);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|s| s.encode_utf16().count() <= 1700));
    }
}

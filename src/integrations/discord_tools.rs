//! Tools a Discord bot's model may call while it replies: music, moderation and games.
//!
//! discord.rs builds one `DiscordToolHost` per message and hands it to the engine as its tool
//! host. Calls from Discord skip the desktop approval dialog, so every call here checks the
//! bot's own settings and Discord itself: the requesting member's and the bot's permissions in
//! the channel, the role hierarchy, and a ✅/❌ confirmation from the requester for anything
//! destructive. Members and roles are looked up in the message's server only.

use crate::{engine::ToolHost, integrations::discord::BotTools};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use serenity::{
    all::{
        ChannelId, CreateAllowedMentions, CreateMessage, EditMember, EditMessage, GetMessages,
        GuildChannel, GuildId, Http, Member, Message, MessageId, PartialGuild, Permissions,
        ReactionType, RoleId, Timestamp, UserId,
    },
    async_trait,
};
use std::{
    cmp::Ordering as CmpOrdering,
    collections::{HashMap, HashSet, hash_map::RandomState},
    hash::BuildHasher,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// How long the requester has to confirm a destructive action.
const CONFIRM_TIME: Duration = Duration::from_secs(60);
const CONFIRM_POLL: Duration = Duration::from_secs(2);
const YES: &str = "✅";
const NO: &str = "❌";
/// Messages one delete_messages call may remove, and how many above that need confirming.
const MAX_DELETE: u64 = 50;
const DELETE_WITHOUT_CONFIRM: u64 = 5;
/// Discord's bulk delete only takes messages younger than 14 days (a minute of margin).
const BULK_DELETE_AGE: i64 = 14 * 24 * 3600 - 60;
const MAX_TIMEOUT_MINUTES: u64 = 10_080;
const MAX_BAN_DELETE_DAYS: u64 = 7;
const MAX_REASON: usize = 200;
/// A trivia question stays open this long; a number game resets after this much quiet.
const TRIVIA_TIME: Duration = Duration::from_secs(120);
const GUESS_IDLE: Duration = Duration::from_secs(900);
const GUESS_MAX: u32 = 100;
/// Channels whose game state is kept per bot; the least recently used is dropped.
const MAX_GAME_CHANNELS: usize = 500;
const MAX_POLL_OPTIONS: usize = 10;
const NUMBER_EMOJI: [&str; MAX_POLL_OPTIONS] = [
    "1\u{fe0f}\u{20e3}",
    "2\u{fe0f}\u{20e3}",
    "3\u{fe0f}\u{20e3}",
    "4\u{fe0f}\u{20e3}",
    "5\u{fe0f}\u{20e3}",
    "6\u{fe0f}\u{20e3}",
    "7\u{fe0f}\u{20e3}",
    "8\u{fe0f}\u{20e3}",
    "9\u{fe0f}\u{20e3}",
    "\u{1f51f}",
];

/// Game ids for `BotTools::games_list` (empty list = all) and their UI labels.
// Not read by the UI yet: the bot card has one Games switch (empty games_list = all).
#[allow(dead_code)]
pub const GAMES: &[(&str, &str)] = &[
    ("dice", "Dice"),
    ("coin", "Coin flip"),
    ("rps", "Rock paper scissors"),
    ("trivia", "Trivia"),
    ("poll", "Polls"),
    ("guess", "Number guess"),
];
const MUSIC_TOOLS: &[&str] = &[
    "play_music",
    "skip_track",
    "stop_music",
    "pause_music",
    "resume_music",
    "show_queue",
];

// ---------------------------------------------------------------------------------------------
// Music bridge (implemented by music.rs)
// ---------------------------------------------------------------------------------------------

/// Who asked for a music action and where. `is_dj` is true when no DJ role is set, or the
/// requester has it (or Administrator / Manage Server). music.rs decides with it whether skip,
/// stop and clear are allowed (the requester may always skip their own track).
#[derive(Clone, Debug)]
pub struct MusicRequest {
    pub guild: GuildId,
    /// Where the request was made (not read by the current MusicControl impl).
    #[allow(dead_code)]
    pub text_channel: ChannelId,
    pub requester: UserId,
    pub requester_name: String,
    /// The requester's voice channel when the caller knows it; None lets music.rs look it up.
    pub voice_channel: Option<ChannelId>,
    pub is_dj: bool,
}

/// Music actions for the tools. The returned text is shown to the model (and so the channel);
/// an error's message should be a short sentence for the user.
#[async_trait]
pub trait MusicControl: Send + Sync {
    async fn play(&self, req: &MusicRequest, query: &str) -> Result<String>;
    async fn skip(&self, req: &MusicRequest) -> Result<String>;
    async fn stop(&self, req: &MusicRequest) -> Result<String>;
    async fn pause(&self, req: &MusicRequest) -> Result<String>;
    async fn resume(&self, req: &MusicRequest) -> Result<String>;
    async fn queue(&self, req: &MusicRequest) -> Result<String>;
}

// ---------------------------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------------------------

/// Everything one message's tool host needs. The requester's member record and permissions
/// are fetched by the host when a tool needs them, so they are always current.
pub struct ToolContext {
    pub http: Arc<Http>,
    /// The bot's tokio runtime. Tool calls arrive on the engine's own thread and block on it.
    pub runtime: tokio::runtime::Handle,
    /// None in DMs: moderation and music are then not offered.
    pub guild: Option<GuildId>,
    pub channel: ChannelId,
    /// The message that triggered the reply; delete_messages only removes older messages.
    pub trigger: Option<MessageId>,
    pub requester: UserId,
    /// Display name for scores and the mod-log.
    pub requester_name: String,
    pub voice_channel: Option<ChannelId>,
    pub tools: BotTools,
    pub music: Option<Arc<dyn MusicControl>>,
    /// The bot's game state (one store per running bot, shared by its messages).
    pub games: Arc<GameStore>,
    /// The reply's cancel flag: a pending confirmation gives up when it is set.
    pub cancel: Arc<AtomicBool>,
}

pub struct DiscordToolHost {
    cx: ToolContext,
    bot_id: Mutex<Option<UserId>>,
}

/// The guild and the channel whose permissions apply (a thread's parent).
struct Scene {
    guild: PartialGuild,
    channel: GuildChannel,
}

impl DiscordToolHost {
    pub fn new(mut cx: ToolContext) -> Self {
        cx.requester_name = clean_name(&cx.requester_name);
        Self {
            cx,
            bot_id: Mutex::new(None),
        }
    }

    fn music_on(&self) -> bool {
        self.cx.tools.music && self.cx.music.is_some() && self.cx.guild.is_some()
    }
    fn mod_on(&self, action: ModAction) -> bool {
        let s = &self.cx.tools.moderation_settings;
        self.cx.tools.moderation
            && self.cx.guild.is_some()
            && match action {
                ModAction::Delete => s.allow_delete,
                ModAction::Timeout => s.allow_timeout,
                ModAction::Kick => s.allow_kick,
                ModAction::Ban => s.allow_ban,
                ModAction::AddRole | ModAction::RemoveRole => s.allow_roles,
            }
    }
    fn game_on(&self, game: &str) -> bool {
        self.cx.tools.games && game_enabled(&self.cx.tools.games_list, game)
    }
    /// Whether a tool is offered (and may be called) for this message.
    pub fn enabled(&self, name: &str) -> bool {
        if MUSIC_TOOLS.contains(&name) {
            return self.music_on();
        }
        if let Some(action) = ModAction::from_tool(name) {
            return self.mod_on(action);
        }
        match game_of(name) {
            Some(game) => self.game_on(game),
            None => false,
        }
    }

    /// Runs async Discord work from the engine's (non-async) thread.
    fn block<T>(&self, fut: impl Future<Output = T>) -> Result<T> {
        if tokio::runtime::Handle::try_current().is_ok() {
            bail!("Discord tools must be called from the engine thread, not the async runtime");
        }
        Ok(self.cx.runtime.block_on(fut))
    }

    async fn bot_id(&self) -> Result<UserId, String> {
        let cached = *self.bot_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = cached {
            return Ok(id);
        }
        let user = self
            .cx
            .http
            .get_current_user()
            .await
            .map_err(|_| "Could not read the bot's own account.".to_string())?;
        *self.bot_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(user.id);
        Ok(user.id)
    }

    async fn scene(&self) -> Result<Scene, String> {
        let guild_id = self.cx.guild.ok_or("This only works in a server.")?;
        let http = &*self.cx.http;
        let guild = http
            .get_guild(guild_id)
            .await
            .map_err(|_| "Could not read this server's roles.")?;
        let mut channel = http
            .get_channel(self.cx.channel)
            .await
            .ok()
            .and_then(|c| c.guild())
            .filter(|c| c.guild_id == guild_id)
            .ok_or("Could not read this channel.")?;
        // A thread has no overwrites of its own; its parent channel's apply.
        if channel.thread_metadata.is_some()
            && let Some(parent) = channel.parent_id
            && let Some(parent) = http.get_channel(parent).await.ok().and_then(|c| c.guild())
        {
            channel = parent;
        }
        Ok(Scene { guild, channel })
    }

    /// A member of this server with their channel permissions and top role.
    async fn standing(&self, scene: &Scene, user: UserId) -> Result<(Member, Standing), String> {
        let member = scene
            .guild
            .id
            .member(&*self.cx.http, user)
            .await
            .map_err(|_| "That user is not a member of this server.")?;
        let perms = scene.guild.user_permissions_in(&scene.channel, &member);
        let guild_perms = scene.guild.member_permissions(&member);
        let rank = top_rank(
            member
                .roles
                .iter()
                .filter_map(|r| scene.guild.roles.get(r))
                .map(|r| role_rank(r.position, r.id.get())),
        );
        let standing = Standing {
            id: user.get(),
            perms,
            guild_perms,
            rank,
            owner: user == scene.guild.owner_id,
        };
        Ok((member, standing))
    }

    /// A member of this server from a mention, ID or (exact) name.
    async fn resolve_member(&self, scene: &Scene, query: &str) -> Result<UserId, String> {
        match parse_user_ref(query).ok_or("Name a member: a mention, ID or name.")? {
            UserRef::Id(id) => Ok(UserId::new(id)),
            UserRef::Name(name) => {
                let found = scene
                    .guild
                    .id
                    .search_members(&*self.cx.http, &name, Some(25))
                    .await
                    .map_err(|_| "Could not search this server's members.")?;
                let candidates: Vec<(u64, Vec<String>)> = found
                    .iter()
                    .map(|m| {
                        let names = [
                            Some(m.user.name.clone()),
                            m.user.global_name.clone(),
                            m.nick.clone(),
                        ];
                        (m.user.id.get(), names.into_iter().flatten().collect())
                    })
                    .collect();
                pick_member(&candidates, &name).map(UserId::new)
            }
        }
    }

    /// Posts a confirmation and waits for the requester's ✅ (true) or ❌ / time-out (false).
    async fn confirm(&self, summary: &str) -> Result<bool, String> {
        let http = &*self.cx.http;
        let text = format!(
            "**Confirm:** {summary}?\n<@{}>, react {YES} to go ahead or {NO} to cancel ({} s).",
            self.cx.requester,
            CONFIRM_TIME.as_secs()
        );
        let message = CreateMessage::new()
            .content(text)
            .allowed_mentions(no_mentions());
        let posted = self
            .cx
            .channel
            .send_message(http, message)
            .await
            .map_err(|_| "Could not post the confirmation message.")?;
        if posted.react(http, emoji(YES)).await.is_err()
            || posted.react(http, emoji(NO)).await.is_err()
        {
            self.note(&posted, summary, "Cancelled: I can't add reactions here.")
                .await;
            return Err(
                "I need Add Reactions and Read Message History here to ask for confirmation."
                    .into(),
            );
        }
        let deadline = Instant::now() + CONFIRM_TIME;
        let decision = loop {
            if self.cx.cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                break None;
            }
            tokio::time::sleep(CONFIRM_POLL).await;
            if self.reacted(&posted, YES).await {
                break Some(true);
            }
            if self.reacted(&posted, NO).await {
                break Some(false);
            }
        };
        let note = match decision {
            Some(true) => "Confirmed.",
            Some(false) => "Cancelled.",
            None => "Timed out: nothing was done.",
        };
        self.note(&posted, summary, note).await;
        Ok(decision == Some(true))
    }
    async fn reacted(&self, message: &Message, reaction: &str) -> bool {
        message
            .reaction_users(&*self.cx.http, emoji(reaction), Some(100), None::<UserId>)
            .await
            .is_ok_and(|users| users.iter().any(|u| u.id == self.cx.requester))
    }
    async fn note(&self, message: &Message, summary: &str, note: &str) {
        let edit = EditMessage::new()
            .content(format!("**{summary}**: {note}"))
            .allowed_mentions(no_mentions());
        let _ = self
            .cx
            .channel
            .edit_message(&*self.cx.http, message.id, edit)
            .await;
    }

    /// Writes one line to the mod-log channel, when it is set and belongs to this server.
    async fn log(&self, scene: &Scene, what: &str, reason: &str) {
        let Some(id) = parse_channel(&self.cx.tools.moderation_settings.log_channel) else {
            return;
        };
        let http = &*self.cx.http;
        let channel = ChannelId::new(id);
        let same_server = http
            .get_channel(channel)
            .await
            .ok()
            .and_then(|c| c.guild())
            .is_some_and(|c| c.guild_id == scene.guild.id);
        if !same_server {
            return;
        }
        let line = format!(
            "[Mod] {} ({}) {what} in <#{}>. Reason: {reason}",
            self.cx.requester_name, self.cx.requester, self.cx.channel
        );
        let message = CreateMessage::new()
            .content(line)
            .allowed_mentions(no_mentions());
        let _ = channel.send_message(http, message).await;
    }

    async fn music(&self, name: &str, args: &Value) -> Result<String, String> {
        let music = self.cx.music.clone().ok_or("Music is off for this bot.")?;
        let guild = self.cx.guild.ok_or("Music only works in a server.")?;
        let is_dj = match name {
            "skip_track" | "stop_music" => self.is_dj().await,
            _ => true,
        };
        let req = MusicRequest {
            guild,
            text_channel: self.cx.channel,
            requester: self.cx.requester,
            requester_name: self.cx.requester_name.clone(),
            voice_channel: self.cx.voice_channel,
            is_dj,
        };
        let result = match name {
            "play_music" => {
                let query = arg_str(args, "query").unwrap_or_default();
                let query = query.trim();
                if query.is_empty() {
                    return Err("Say what to play: a song name or a link.".into());
                }
                if query.chars().count() > 300 {
                    return Err("That search is too long (300 characters at most).".into());
                }
                music.play(&req, query).await
            }
            "skip_track" => music.skip(&req).await,
            "stop_music" => music.stop(&req).await,
            "pause_music" => music.pause(&req).await,
            "resume_music" => music.resume(&req).await,
            "show_queue" => music.queue(&req).await,
            _ => return Err(format!("Unknown music tool: {name}")),
        };
        result.map_err(|e| format!("{e:#}"))
    }
    async fn is_dj(&self) -> bool {
        let dj_role = self.cx.tools.music_tools.dj_role.trim().to_owned();
        if dj_role.is_empty() {
            return true;
        }
        let Ok(scene) = self.scene().await else {
            return false;
        };
        let Ok((member, standing)) = self.standing(&scene, self.cx.requester).await else {
            return false;
        };
        let roles: Vec<(u64, String)> = member
            .roles
            .iter()
            .filter_map(|r| scene.guild.roles.get(r))
            .map(|r| (r.id.get(), r.name.clone()))
            .collect();
        dj_allowed(&dj_role, &roles, standing.perms)
    }

    async fn moderate(&self, action: ModAction, args: &Value) -> Result<String, String> {
        let scene = self.scene().await?;
        let http = &*self.cx.http;
        let (_, requester) = self.standing(&scene, self.cx.requester).await?;
        let bot_id = self.bot_id().await?;
        let (_, bot) = self
            .standing(&scene, bot_id)
            .await
            .map_err(|_| "Could not read the bot's own roles in this server.".to_string())?;
        let reason = clean_reason(arg_str(args, "reason").as_deref());
        let target_ref = arg_str(args, "user").filter(|u| !u.trim().is_empty());
        let target = match (action, target_ref) {
            (ModAction::Delete, None) => None,
            (_, None) => return Err("Say which member.".into()),
            (_, Some(query)) => {
                let id = self.resolve_member(&scene, &query).await?;
                Some(self.standing(&scene, id).await?)
            }
        };
        check_moderation(
            action,
            self.mod_on(action),
            &requester,
            &bot,
            target.as_ref().map(|(_, s)| s),
        )?;
        let label = target
            .as_ref()
            .map(|(m, _)| format!("{} ({})", clean_name(&m.user.name), m.user.id))
            .unwrap_or_default();
        let target_id = target.as_ref().map(|(m, _)| m.user.id);
        let not_confirmed = || Ok("Not done: the requester did not confirm.".to_string());
        let refused = |_: serenity::Error| {
            "Discord refused the action. Check the bot's permissions and role position.".to_string()
        };
        let guild = scene.guild.id;
        match action {
            ModAction::Delete => {
                let count = arg_u64(args, "count").unwrap_or(0);
                if !(1..=MAX_DELETE).contains(&count) {
                    return Err(format!("count must be 1 to {MAX_DELETE}."));
                }
                let summary = match target_id {
                    Some(_) => format!("Delete the last {count} message(s) from {label}"),
                    None => format!("Delete the last {count} message(s) in this channel"),
                };
                if needs_confirmation(action, count) && !self.confirm(&summary).await? {
                    return not_confirmed();
                }
                let mut query = GetMessages::new().limit(100);
                if let Some(trigger) = self.cx.trigger {
                    query = query.before(trigger);
                }
                let messages = self
                    .cx
                    .channel
                    .messages(http, query)
                    .await
                    .map_err(|_| "Could not read this channel's messages.")?;
                let cutoff = Timestamp::now().unix_timestamp() - BULK_DELETE_AGE;
                let ids: Vec<MessageId> = messages
                    .iter()
                    .filter(|m| !m.pinned && m.timestamp.unix_timestamp() > cutoff)
                    .filter(|m| target_id.is_none_or(|t| m.author.id == t))
                    // Without the trigger to anchor on, keep this reply's own messages.
                    .filter(|m| self.cx.trigger.is_some() || m.author.id != bot_id)
                    .take(count as usize)
                    .map(|m| m.id)
                    .collect();
                if ids.is_empty() {
                    return Ok("No messages to delete (pinned ones and those older than 14 days are skipped).".into());
                }
                self.cx
                    .channel
                    .delete_messages(http, &ids)
                    .await
                    .map_err(refused)?;
                let what = match target_id {
                    Some(_) => format!("deleted {} message(s) from {label}", ids.len()),
                    None => format!("deleted {} message(s)", ids.len()),
                };
                self.log(&scene, &what, &reason).await;
                Ok(format!("Done: {what}."))
            }
            ModAction::Timeout => {
                let Some(user) = target_id else {
                    return Err("Say which member.".into());
                };
                let minutes = arg_u64(args, "minutes")
                    .ok_or("Say how many minutes (0 removes a timeout).")?;
                if minutes > MAX_TIMEOUT_MINUTES {
                    return Err(format!(
                        "A timeout can be at most {MAX_TIMEOUT_MINUTES} minutes (7 days)."
                    ));
                }
                if minutes > 0
                    && !self
                        .confirm(&format!("Time out {label} for {minutes} minute(s)"))
                        .await?
                {
                    return not_confirmed();
                }
                let edit = if minutes == 0 {
                    EditMember::new().enable_communication()
                } else {
                    let until = Timestamp::now().unix_timestamp() + minutes as i64 * 60;
                    let until = Timestamp::from_unix_timestamp(until)
                        .map_err(|_| "Invalid timeout length.")?;
                    EditMember::new().disable_communication_until_datetime(until)
                };
                let edit = edit.audit_log_reason(&reason);
                guild.edit_member(http, user, edit).await.map_err(refused)?;
                let what = if minutes == 0 {
                    format!("removed the timeout of {label}")
                } else {
                    format!("timed out {label} for {minutes} minute(s)")
                };
                self.log(&scene, &what, &reason).await;
                Ok(format!("Done: {what}."))
            }
            ModAction::Kick => {
                let Some(user) = target_id else {
                    return Err("Say which member.".into());
                };
                if !self.confirm(&format!("Kick {label}")).await? {
                    return not_confirmed();
                }
                guild
                    .kick_with_reason(http, user, &reason)
                    .await
                    .map_err(refused)?;
                let what = format!("kicked {label}");
                self.log(&scene, &what, &reason).await;
                Ok(format!("Done: {what}."))
            }
            ModAction::Ban => {
                let Some(user) = target_id else {
                    return Err("Say which member.".into());
                };
                let days = arg_u64(args, "delete_days").unwrap_or(0);
                if days > MAX_BAN_DELETE_DAYS {
                    return Err(format!("delete_days can be at most {MAX_BAN_DELETE_DAYS}."));
                }
                let summary = format!("Ban {label} (delete {days} day(s) of their messages)");
                if !self.confirm(&summary).await? {
                    return not_confirmed();
                }
                guild
                    .ban_with_reason(http, user, days as u8, reason.as_str())
                    .await
                    .map_err(refused)?;
                let what = format!("banned {label}");
                self.log(&scene, &what, &reason).await;
                Ok(format!("Done: {what}."))
            }
            ModAction::AddRole | ModAction::RemoveRole => {
                let Some((member, _)) = target.as_ref() else {
                    return Err("Say which member.".into());
                };
                let query = arg_str(args, "role").unwrap_or_default();
                let roles: Vec<(u64, String)> = scene
                    .guild
                    .roles
                    .values()
                    .filter(|r| r.id.get() != guild.get())
                    .map(|r| (r.id.get(), r.name.clone()))
                    .collect();
                let role_id = RoleId::new(pick_role(&roles, &query)?);
                let role = scene
                    .guild
                    .roles
                    .get(&role_id)
                    .ok_or("That role is not in this server.")?;
                check_role_change(
                    role_rank(role.position, role.id.get()),
                    role.permissions,
                    role.managed,
                    &requester,
                    &bot,
                )?;
                let adding = action == ModAction::AddRole;
                let has = member.roles.contains(&role_id);
                let role_name = clean_name(&role.name);
                if adding && has {
                    return Ok(format!("{label} already has {role_name}."));
                }
                if !adding && !has {
                    return Ok(format!("{label} doesn't have {role_name}."));
                }
                let summary = if adding {
                    format!("Give {role_name} to {label}")
                } else {
                    format!("Remove {role_name} from {label}")
                };
                if !self.confirm(&summary).await? {
                    return not_confirmed();
                }
                let user = member.user.id;
                let done = if adding {
                    http.add_member_role(guild, user, role_id, Some(reason.as_str()))
                        .await
                } else {
                    http.remove_member_role(guild, user, role_id, Some(reason.as_str()))
                        .await
                };
                done.map_err(refused)?;
                let what = if adding {
                    format!("gave {role_name} to {label}")
                } else {
                    format!("removed {role_name} from {label}")
                };
                self.log(&scene, &what, &reason).await;
                Ok(format!("Done: {what}."))
            }
        }
    }

    async fn poll(&self, args: &Value) -> Result<String, String> {
        let question = arg_str(args, "question").unwrap_or_default();
        let (question, options) = validate_poll(&question, &list_arg(args, "options"))?;
        let mut text = format!("**Poll:** {question}\n");
        for (emoji, option) in NUMBER_EMOJI.iter().zip(&options) {
            text.push_str(&format!("{emoji} {option}\n"));
        }
        text.push_str(&format!(
            "Asked by {}. React to vote.",
            self.cx.requester_name
        ));
        let reactions: Vec<ReactionType> = NUMBER_EMOJI[..options.len()]
            .iter()
            .map(|e| emoji(e))
            .collect();
        let message = CreateMessage::new()
            .content(text)
            .allowed_mentions(no_mentions())
            .reactions(reactions);
        self.cx
            .channel
            .send_message(&*self.cx.http, message)
            .await
            .map_err(
                |_| "Could not post the poll (the bot needs Send Messages and Add Reactions).",
            )?;
        Ok(format!(
            "Posted the poll \"{question}\" with {} options. Don't repeat it; tell people to vote with the reactions.",
            options.len()
        ))
    }

    fn game(&self, name: &str, args: &Value) -> Result<String, String> {
        let games = &self.cx.games;
        let (channel, user, who) = (
            self.cx.channel.get(),
            self.cx.requester.get(),
            self.cx.requester_name.as_str(),
        );
        let now = Instant::now();
        match name {
            "roll_dice" => {
                let (sides, count) = validate_dice(arg_u64(args, "sides"), arg_u64(args, "count"))?;
                let rolls = games.with_rng(|rng| roll(rng, sides, count));
                let total: u32 = rolls.iter().sum();
                let list: Vec<String> = rolls.iter().map(u32::to_string).collect();
                Ok(if count == 1 {
                    format!("{who} rolled a d{sides}: {total}")
                } else {
                    format!(
                        "{who} rolled {count}d{sides}: {} (total {total})",
                        list.join(", ")
                    )
                })
            }
            "flip_coin" => {
                let heads = games.with_rng(|rng| rng.below(2) == 0);
                Ok(format!(
                    "{who} flipped a coin: {}",
                    if heads { "heads" } else { "tails" }
                ))
            }
            "rock_paper_scissors" => {
                let choice = arg_str(args, "choice").unwrap_or_default();
                let mine = parse_rps(&choice).ok_or("Choose rock, paper or scissors.")?;
                let bot = games.with_rng(|rng| Rps::ALL[rng.below(3) as usize]);
                let outcome = match rps_result(mine, bot) {
                    Outcome::Win => format!("{who} wins!"),
                    Outcome::Lose => "I win!".to_string(),
                    Outcome::Draw => "It's a draw.".to_string(),
                };
                Ok(format!(
                    "{who} chose {}, I chose {}. {outcome}",
                    mine.name(),
                    bot.name()
                ))
            }
            "start_trivia" => {
                Ok(games.start_trivia(channel, arg_str(args, "topic").as_deref(), now))
            }
            "answer_trivia" => {
                let answer = arg_str(args, "answer").unwrap_or_default();
                Ok(games.answer_trivia(channel, user, who, &answer, now))
            }
            "guess_number" => {
                let guess = arg_i64(args, "guess").ok_or("Guess a whole number from 1 to 100.")?;
                games.guess_number(channel, user, who, guess, now)
            }
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}

impl ToolHost for DiscordToolHost {
    fn definitions(&self) -> Vec<Value> {
        definitions()
            .into_iter()
            .filter(|(name, _)| self.enabled(name))
            .map(|(_, definition)| definition)
            .collect()
    }
    fn call(&self, name: &str, args: &Value) -> Result<String> {
        if !self.enabled(name) {
            bail!("The tool {name} is not available here.");
        }
        let args = normalize_args(args);
        let outcome = if MUSIC_TOOLS.contains(&name) {
            self.block(self.music(name, &args))?
        } else if let Some(action) = ModAction::from_tool(name) {
            self.block(self.moderate(action, &args))?
        } else if name == "start_poll" {
            self.block(self.poll(&args))?
        } else {
            self.game(name, &args)
        };
        outcome.map_err(anyhow::Error::msg)
    }
}

/// Tool definitions (OpenAI function format), keyed by name.
fn definitions() -> Vec<(&'static str, Value)> {
    fn tool(
        name: &'static str,
        description: &str,
        properties: Value,
        required: &[&str],
    ) -> (&'static str, Value) {
        let parameters = json!({"type":"object","properties":properties,"required":required});
        (
            name,
            json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}}),
        )
    }
    let none = || json!({});
    let user =
        json!({"type":"string","description":"The member: a mention, user ID or exact name"});
    let reason = json!({"type":"string","description":"Short reason for the audit log"});
    vec![
        tool(
            "play_music",
            "Play a song in the requester's voice channel, or add it to the queue. Takes a search or a YouTube link.",
            json!({"query":{"type":"string","description":"Song name or link"}}),
            &["query"],
        ),
        tool("skip_track", "Skip the current song.", none(), &[]),
        tool(
            "stop_music",
            "Stop playback and clear the queue.",
            none(),
            &[],
        ),
        tool("pause_music", "Pause playback.", none(), &[]),
        tool("resume_music", "Resume paused playback.", none(), &[]),
        tool(
            "show_queue",
            "Show the current song and the queue.",
            none(),
            &[],
        ),
        tool(
            "delete_messages",
            "Delete recent messages in this channel, optionally only one member's. More than 5 needs the requester's confirmation.",
            json!({"count":{"type":"integer","minimum":1,"maximum":MAX_DELETE},"user":user,"reason":reason}),
            &["count"],
        ),
        tool(
            "timeout_member",
            "Time out a member (they can't talk) for some minutes; 0 removes a timeout. The requester must confirm with a reaction.",
            json!({"user":user,"minutes":{"type":"integer","minimum":0,"maximum":MAX_TIMEOUT_MINUTES},"reason":reason}),
            &["user", "minutes"],
        ),
        tool(
            "kick_member",
            "Kick a member from the server (the requester must confirm).",
            json!({"user":user,"reason":reason}),
            &["user"],
        ),
        tool(
            "ban_member",
            "Ban a member (the requester must confirm).",
            json!({"user":user,"reason":reason,"delete_days":{"type":"integer","minimum":0,"maximum":MAX_BAN_DELETE_DAYS,"description":"Days of their messages to delete"}}),
            &["user"],
        ),
        tool(
            "add_role",
            "Give a member a role (the requester must confirm).",
            json!({"user":user,"role":{"type":"string","description":"Role mention, ID or exact name"},"reason":reason}),
            &["user", "role"],
        ),
        tool(
            "remove_role",
            "Take a role from a member (the requester must confirm).",
            json!({"user":user,"role":{"type":"string","description":"Role mention, ID or exact name"},"reason":reason}),
            &["user", "role"],
        ),
        tool(
            "roll_dice",
            "Roll dice.",
            json!({"sides":{"type":"integer","minimum":2,"maximum":1000},"count":{"type":"integer","minimum":1,"maximum":20}}),
            &[],
        ),
        tool("flip_coin", "Flip a coin.", none(), &[]),
        tool(
            "rock_paper_scissors",
            "Play rock paper scissors against the bot.",
            json!({"choice":{"type":"string","enum":["rock","paper","scissors"]}}),
            &["choice"],
        ),
        tool(
            "start_trivia",
            "Start a multiple-choice trivia question in this channel.",
            json!({"topic":{"type":"string","description":"Optional: science, space, geography, history, tech, animals, games or music"}}),
            &[],
        ),
        tool(
            "answer_trivia",
            "Answer the open trivia question for the requester (one try each).",
            json!({"answer":{"type":"string","description":"A, B, C, D or the answer text"}}),
            &["answer"],
        ),
        tool(
            "start_poll",
            "Post a poll people vote on with reactions.",
            json!({"question":{"type":"string"},"options":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":MAX_POLL_OPTIONS}}),
            &["question", "options"],
        ),
        tool(
            "guess_number",
            "Guess the channel's secret number from 1 to 100 (a new game starts if none is running).",
            json!({"guess":{"type":"integer","minimum":1,"maximum":GUESS_MAX}}),
            &["guess"],
        ),
    ]
}

/// The game id a game tool belongs to.
fn game_of(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "roll_dice" => "dice",
        "flip_coin" => "coin",
        "rock_paper_scissors" => "rps",
        "start_trivia" | "answer_trivia" => "trivia",
        "start_poll" => "poll",
        "guess_number" => "guess",
        _ => return None,
    })
}
/// An empty list enables every game.
pub fn game_enabled(list: &[String], game: &str) -> bool {
    list.iter().all(|g| g.trim().is_empty())
        || list.iter().any(|g| g.trim().eq_ignore_ascii_case(game))
}

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
        .replied_user(false)
}
fn emoji(text: &str) -> ReactionType {
    ReactionType::Unicode(text.to_owned())
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

/// Tool arguments as an object: models send an object, a JSON string of one, or nothing.
pub fn normalize_args(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}
fn arg_str(args: &Value, key: &str) -> Option<String> {
    match &args[key] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}
fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    match &args[key] {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 1e15)
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    arg_i64(args, key).and_then(|n| u64::try_from(n).ok())
}
/// A list argument: an array of strings, or one string split on newlines, '|' or ';'.
fn list_arg(args: &Value, key: &str) -> Vec<String> {
    match &args[key] {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) => {
            let sep: &[char] = if s.contains(['\n', '|', ';']) {
                &['\n', '|', ';']
            } else {
                &[',']
            };
            s.split(sep).map(str::to_owned).collect()
        }
        _ => vec![],
    }
}
/// One line, no control characters, at most `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    flat.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}
fn clean_name(name: &str) -> String {
    match one_line(name, 32).as_str() {
        "" => "Someone".into(),
        name => name.into(),
    }
}
fn clean_reason(reason: Option<&str>) -> String {
    match one_line(reason.unwrap_or_default(), MAX_REASON).as_str() {
        "" => "No reason given".into(),
        reason => reason.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Members and roles
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum UserRef {
    Id(u64),
    Name(String),
}
/// A snowflake on its own: digits only, at least 15 of them (shorter numbers are names).
fn parse_snowflake(text: &str) -> Option<u64> {
    let text = text.trim();
    (text.len() >= 15 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse::<u64>().ok())
        .flatten()
        .filter(|&id| id > 0)
}
/// `<@id>`, `<@!id>`, a bare ID, or a name (a leading '@' is dropped).
pub fn parse_user_ref(text: &str) -> Option<UserRef> {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix("<@").and_then(|t| t.strip_suffix('>')) {
        let inner = inner.strip_prefix('!').unwrap_or(inner);
        return inner
            .parse::<u64>()
            .ok()
            .filter(|&id| id > 0)
            .map(UserRef::Id);
    }
    if let Some(id) = parse_snowflake(text) {
        return Some(UserRef::Id(id));
    }
    let name = text.strip_prefix('@').unwrap_or(text).trim();
    // A legacy "name#1234" tag: the name part is what members are searched by.
    let name = match name.rsplit_once('#') {
        Some((base, tag))
            if !base.is_empty() && tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    };
    let count = name.chars().count();
    ((1..=64).contains(&count) && !name.contains(['<', '>'])).then(|| UserRef::Name(name.into()))
}
/// The one member whose user name, display name or nickname is `name` (case-insensitive).
pub fn pick_member(candidates: &[(u64, Vec<String>)], name: &str) -> Result<u64, String> {
    let wanted = name.trim().to_lowercase();
    let mut ids: Vec<u64> = candidates
        .iter()
        .filter(|(_, names)| names.iter().any(|n| n.trim().to_lowercase() == wanted))
        .map(|(id, _)| *id)
        .filter(|&id| id > 0)
        .collect();
    ids.dedup();
    match ids.as_slice() {
        [id] => Ok(*id),
        [] => Err(format!(
            "No member named \"{}\" in this server.",
            one_line(name, 64)
        )),
        _ => Err(format!(
            "Several members are called \"{}\"; use a mention or user ID.",
            one_line(name, 64)
        )),
    }
}
/// A server channel ID from `<#id>` or digits.
fn parse_channel(text: &str) -> Option<u64> {
    let text = text.trim();
    let text = text
        .strip_prefix("<#")
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(text);
    text.parse::<u64>().ok().filter(|&id| id > 0)
}
/// A role from `<@&id>`, its ID or its exact name (case-insensitive) among `roles`.
pub fn pick_role(roles: &[(u64, String)], query: &str) -> Result<u64, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Say which role.".into());
    }
    let as_id = query
        .strip_prefix("<@&")
        .and_then(|t| t.strip_suffix('>'))
        .and_then(|t| t.parse::<u64>().ok())
        .or_else(|| parse_snowflake(query));
    if let Some(id) = as_id {
        return roles
            .iter()
            .find(|(r, _)| *r == id)
            .map(|(r, _)| *r)
            .ok_or_else(|| "That role is not in this server.".to_string());
    }
    let wanted = query
        .strip_prefix('@')
        .unwrap_or(query)
        .trim()
        .to_lowercase();
    let found: Vec<u64> = roles
        .iter()
        .filter(|(_, name)| name.trim().to_lowercase() == wanted)
        .map(|(id, _)| *id)
        .collect();
    match found.as_slice() {
        [id] => Ok(*id),
        [] => Err(format!(
            "No role named \"{}\" in this server.",
            one_line(query, 64)
        )),
        _ => Err("Several roles have that name; use a role mention or ID.".into()),
    }
}

// ---------------------------------------------------------------------------------------------
// Permission decisions
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModAction {
    Delete,
    Timeout,
    Kick,
    Ban,
    AddRole,
    RemoveRole,
}
impl ModAction {
    pub fn from_tool(name: &str) -> Option<Self> {
        Some(match name {
            "delete_messages" => Self::Delete,
            "timeout_member" => Self::Timeout,
            "kick_member" => Self::Kick,
            "ban_member" => Self::Ban,
            "add_role" => Self::AddRole,
            "remove_role" => Self::RemoveRole,
            _ => return None,
        })
    }
    /// The Discord permission both the requester and the bot need in the channel.
    pub fn permission(self) -> (Permissions, &'static str) {
        match self {
            Self::Delete => (Permissions::MANAGE_MESSAGES, "Manage Messages"),
            Self::Timeout => (Permissions::MODERATE_MEMBERS, "Timeout Members"),
            Self::Kick => (Permissions::KICK_MEMBERS, "Kick Members"),
            Self::Ban => (Permissions::BAN_MEMBERS, "Ban Members"),
            Self::AddRole | Self::RemoveRole => (Permissions::MANAGE_ROLES, "Manage Roles"),
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Delete => "delete messages",
            Self::Timeout => "time out members",
            Self::Kick => "kick members",
            Self::Ban => "ban members",
            Self::AddRole | Self::RemoveRole => "change roles",
        }
    }
}

/// Discord orders roles by position, then lower ID first; a higher rank is higher up.
pub type Rank = (u16, u64);
pub fn role_rank(position: u16, id: u64) -> Rank {
    (position, u64::MAX - id)
}
/// A member's highest role (members with no roles rank below every role).
pub fn top_rank(roles: impl IntoIterator<Item = Rank>) -> Rank {
    roles.into_iter().max().unwrap_or((0, 0))
}

/// A member as far as moderation is concerned.
#[derive(Clone, Copy, Debug)]
pub struct Standing {
    pub id: u64,
    /// Permissions in the channel (serenity grants owners and Administrators all of them).
    pub perms: Permissions,
    /// Server-wide permissions from roles only; channel overwrites can't grant these.
    pub guild_perms: Permissions,
    pub rank: Rank,
    pub owner: bool,
}
fn has(perms: Permissions, wanted: Permissions) -> bool {
    perms.contains(Permissions::ADMINISTRATOR) || perms.contains(wanted)
}

/// Whether `requester` may have the bot do `action` (to `target`): enabled in the bot's
/// settings, both the requester and the bot hold the permission, and the target is someone
/// else, not the owner or the bot, and below both of them.
pub fn check_moderation(
    action: ModAction,
    enabled: bool,
    requester: &Standing,
    bot: &Standing,
    target: Option<&Standing>,
) -> Result<(), String> {
    if !enabled {
        return Err(format!(
            "This bot is not allowed to {} (turned off in its settings).",
            action.verb()
        ));
    }
    let (perm, perm_name) = action.permission();
    // Only deleting messages is per channel; the rest act server-wide, so a channel
    // overwrite must not be enough.
    let per_channel = matches!(action, ModAction::Delete);
    let (req_perms, bot_perms, place) = if per_channel {
        (requester.perms, bot.perms, "this channel")
    } else {
        (requester.guild_perms, bot.guild_perms, "this server")
    };
    if !requester.owner && !has(req_perms, perm) {
        return Err(format!("You need the {perm_name} permission in {place}."));
    }
    if !bot.owner && !has(bot_perms, perm) {
        return Err(format!("I need the {perm_name} permission in {place}."));
    }
    let Some(target) = target else {
        return Ok(());
    };
    if target.id == requester.id {
        return Err("You can't use this on yourself.".into());
    }
    if target.id == bot.id {
        return Err("I can't use this on myself.".into());
    }
    if target.owner {
        return Err("Nobody can use this on the server owner.".into());
    }
    if !requester.owner && target.rank >= requester.rank {
        return Err("That member's top role is at or above yours.".into());
    }
    if !bot.owner && target.rank >= bot.rank {
        return Err("That member's top role is at or above mine. Move my role higher.".into());
    }
    Ok(())
}

/// Whether a role may be given or taken: below both the requester's and the bot's top roles,
/// not managed by an integration, and granting nothing the requester lacks.
pub fn check_role_change(
    role: Rank,
    role_perms: Permissions,
    managed: bool,
    requester: &Standing,
    bot: &Standing,
) -> Result<(), String> {
    if managed {
        return Err("That role is managed by an integration or bot and can't be assigned.".into());
    }
    if !requester.owner && role >= requester.rank {
        return Err("That role is at or above your top role.".into());
    }
    if !bot.owner && role >= bot.rank {
        return Err("That role is at or above my top role. Move my role higher.".into());
    }
    let elevated = !requester.owner
        && !requester.guild_perms.contains(Permissions::ADMINISTRATOR)
        && !requester.guild_perms.contains(role_perms);
    if elevated {
        return Err("That role has permissions you don't have.".into());
    }
    Ok(())
}

/// Timeouts, kicks, bans, role changes and deleting more than a few messages need the
/// requester's ✅.
pub fn needs_confirmation(action: ModAction, count: u64) -> bool {
    match action {
        ModAction::Delete => count > DELETE_WITHOUT_CONFIRM,
        ModAction::Timeout => true,
        ModAction::Kick | ModAction::Ban | ModAction::AddRole | ModAction::RemoveRole => true,
    }
}

/// DJ check: no DJ role set, the member has it (by ID, mention or name), or is an
/// Administrator / Manage Server.
pub fn dj_allowed(dj_role: &str, member_roles: &[(u64, String)], perms: Permissions) -> bool {
    let dj_role = dj_role.trim();
    if dj_role.is_empty()
        || perms.contains(Permissions::ADMINISTRATOR)
        || perms.contains(Permissions::MANAGE_GUILD)
    {
        return true;
    }
    let id = dj_role
        .strip_prefix("<@&")
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(dj_role)
        .parse::<u64>()
        .ok();
    let name = dj_role.strip_prefix('@').unwrap_or(dj_role).to_lowercase();
    member_roles
        .iter()
        .any(|(rid, rname)| Some(*rid) == id || rname.trim().to_lowercase() == name)
}

// ---------------------------------------------------------------------------------------------
// Games
// ---------------------------------------------------------------------------------------------

/// splitmix64: small, fast and good enough for games.
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Self::new(RandomState::new().hash_one(nanos))
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// 0..n (0 when n is 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

/// Dice size and count with defaults (one d6) and limits (2–1000 sides, 1–20 dice).
pub fn validate_dice(sides: Option<u64>, count: Option<u64>) -> Result<(u32, u32), String> {
    let sides = sides.unwrap_or(6);
    let count = count.unwrap_or(1);
    if !(2..=1000).contains(&sides) {
        return Err("Dice need 2 to 1000 sides.".into());
    }
    if !(1..=20).contains(&count) {
        return Err("Roll 1 to 20 dice.".into());
    }
    Ok((sides as u32, count as u32))
}
pub fn roll(rng: &mut Rng, sides: u32, count: u32) -> Vec<u32> {
    (0..count)
        .map(|_| 1 + rng.below(u64::from(sides)) as u32)
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rps {
    Rock,
    Paper,
    Scissors,
}
impl Rps {
    const ALL: [Rps; 3] = [Rps::Rock, Rps::Paper, Rps::Scissors];
    fn name(self) -> &'static str {
        match self {
            Rps::Rock => "rock",
            Rps::Paper => "paper",
            Rps::Scissors => "scissors",
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Win,
    Lose,
    Draw,
}
pub fn parse_rps(text: &str) -> Option<Rps> {
    match text.trim().to_lowercase().trim_end_matches(['!', '.']) {
        "rock" | "r" | "stone" | "🪨" => Some(Rps::Rock),
        "paper" | "p" | "📄" => Some(Rps::Paper),
        "scissors" | "scissor" | "s" | "✂️" | "✂" => Some(Rps::Scissors),
        _ => None,
    }
}
/// The outcome for the player.
pub fn rps_result(player: Rps, bot: Rps) -> Outcome {
    match (player, bot) {
        (a, b) if a == b => Outcome::Draw,
        (Rps::Rock, Rps::Scissors) | (Rps::Paper, Rps::Rock) | (Rps::Scissors, Rps::Paper) => {
            Outcome::Win
        }
        _ => Outcome::Lose,
    }
}

/// A question, poll options cleaned: 1–300 characters; 2–10 distinct options of ≤ 100.
pub fn validate_poll(question: &str, options: &[String]) -> Result<(String, Vec<String>), String> {
    let question = one_line(question, 301);
    if question.is_empty() || question.chars().count() > 300 {
        return Err("The poll question must be 1 to 300 characters.".into());
    }
    let mut cleaned: Vec<String> = vec![];
    for option in options {
        let option = one_line(option, 101);
        if option.is_empty() {
            continue;
        }
        if option.chars().count() > 100 {
            return Err("Poll options can be at most 100 characters.".into());
        }
        if !cleaned
            .iter()
            .any(|o| o.to_lowercase() == option.to_lowercase())
        {
            cleaned.push(option);
        }
    }
    if !(2..=MAX_POLL_OPTIONS).contains(&cleaned.len()) {
        return Err(format!(
            "A poll needs 2 to {MAX_POLL_OPTIONS} different options."
        ));
    }
    Ok((question, cleaned))
}

/// Higher (`Greater`: the secret is higher than the guess), lower or right.
pub fn guess_hint(secret: u32, guess: u32) -> CmpOrdering {
    secret.cmp(&guess)
}

pub struct Question {
    pub topic: &'static str,
    pub text: &'static str,
    pub choices: [&'static str; 4],
    pub answer: usize,
}
const fn q(
    topic: &'static str,
    text: &'static str,
    choices: [&'static str; 4],
    answer: usize,
) -> Question {
    Question {
        topic,
        text,
        choices,
        answer,
    }
}
pub const TRIVIA: &[Question] = &[
    q(
        "science",
        "What is the chemical symbol for gold?",
        ["Ag", "Au", "Gd", "Go"],
        1,
    ),
    q(
        "science",
        "Which gas do plants mainly take in for photosynthesis?",
        ["Oxygen", "Nitrogen", "Carbon dioxide", "Helium"],
        2,
    ),
    q(
        "science",
        "How many bones are in an adult human body?",
        ["196", "206", "216", "226"],
        1,
    ),
    q(
        "science",
        "What is the hardest natural material?",
        ["Quartz", "Diamond", "Granite", "Iron"],
        1,
    ),
    q(
        "science",
        "What is the freezing point of water in Fahrenheit?",
        ["0", "32", "100", "212"],
        1,
    ),
    q(
        "space",
        "Which planet is known as the Red Planet?",
        ["Venus", "Mars", "Jupiter", "Mercury"],
        1,
    ),
    q(
        "space",
        "What is the largest planet in the Solar System?",
        ["Saturn", "Neptune", "Jupiter", "Earth"],
        2,
    ),
    q(
        "space",
        "What is the closest star to Earth?",
        ["Proxima Centauri", "The Sun", "Sirius", "Polaris"],
        1,
    ),
    q(
        "space",
        "Who was the first person to walk on the Moon?",
        [
            "Buzz Aldrin",
            "Yuri Gagarin",
            "Neil Armstrong",
            "Michael Collins",
        ],
        2,
    ),
    q(
        "geography",
        "What is the capital of Australia?",
        ["Sydney", "Melbourne", "Canberra", "Perth"],
        2,
    ),
    q(
        "geography",
        "What is the largest ocean on Earth?",
        ["Atlantic", "Indian", "Arctic", "Pacific"],
        3,
    ),
    q(
        "geography",
        "What is the capital of Canada?",
        ["Toronto", "Ottawa", "Vancouver", "Montreal"],
        1,
    ),
    q(
        "geography",
        "Which country has the largest land area?",
        ["Canada", "China", "Russia", "United States"],
        2,
    ),
    q(
        "geography",
        "Mount Everest sits on the border of Nepal and which country?",
        ["India", "China", "Bhutan", "Pakistan"],
        1,
    ),
    q(
        "history",
        "In which year did World War II end?",
        ["1943", "1944", "1945", "1946"],
        2,
    ),
    q(
        "history",
        "Who was the first President of the United States?",
        [
            "Thomas Jefferson",
            "George Washington",
            "John Adams",
            "Abraham Lincoln",
        ],
        1,
    ),
    q(
        "history",
        "The Great Pyramid of Giza was built for which pharaoh?",
        ["Khufu", "Tutankhamun", "Ramesses II", "Cleopatra"],
        0,
    ),
    q(
        "history",
        "In which year did the Berlin Wall fall?",
        ["1987", "1989", "1991", "1993"],
        1,
    ),
    q(
        "tech",
        "What does CPU stand for?",
        [
            "Central Processing Unit",
            "Computer Personal Unit",
            "Central Program Utility",
            "Core Processing Unit",
        ],
        0,
    ),
    q(
        "tech",
        "Which organisation started the Rust programming language?",
        ["Google", "Mozilla", "Microsoft", "Apple"],
        1,
    ),
    q(
        "tech",
        "How many bits are in a byte?",
        ["4", "8", "16", "32"],
        1,
    ),
    q(
        "tech",
        "What does HTTP stand for?",
        [
            "HyperText Transfer Protocol",
            "High Transfer Text Protocol",
            "Hyperlink Text Transport Protocol",
            "Host Transfer Text Process",
        ],
        0,
    ),
    q(
        "animals",
        "What is the largest mammal?",
        ["African elephant", "Blue whale", "Giraffe", "Sperm whale"],
        1,
    ),
    q(
        "animals",
        "How many legs does a spider have?",
        ["6", "8", "10", "12"],
        1,
    ),
    q(
        "animals",
        "What is a group of lions called?",
        ["Pack", "Herd", "Pride", "Flock"],
        2,
    ),
    q(
        "animals",
        "What is the fastest land animal?",
        ["Lion", "Cheetah", "Pronghorn", "Horse"],
        1,
    ),
    q(
        "games",
        "In chess, which piece only moves diagonally?",
        ["Rook", "Knight", "Bishop", "King"],
        2,
    ),
    q(
        "games",
        "Which company makes the PlayStation?",
        ["Nintendo", "Sony", "Microsoft", "Sega"],
        1,
    ),
    q(
        "games",
        "In Minecraft, which block makes a Nether portal frame?",
        ["Obsidian", "Bedrock", "Netherrack", "Cobblestone"],
        0,
    ),
    q(
        "games",
        "How many squares are on a chess board?",
        ["32", "48", "64", "81"],
        2,
    ),
    q(
        "music",
        "How many keys does a standard piano have?",
        ["76", "82", "88", "96"],
        2,
    ),
    q(
        "music",
        "How many lines does a standard musical staff have?",
        ["4", "5", "6", "7"],
        1,
    ),
];
const LETTERS: [&str; 4] = ["A", "B", "C", "D"];

/// The topic a player's word refers to, if any.
fn topic_key(topic: &str) -> Option<&'static str> {
    let t = topic.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    const ALIASES: &[(&str, &str)] = &[
        ("science", "science"),
        ("chemistry", "science"),
        ("biology", "science"),
        ("physics", "science"),
        ("space", "space"),
        ("astronomy", "space"),
        ("planet", "space"),
        ("geography", "geography"),
        ("countries", "geography"),
        ("capital", "geography"),
        ("history", "history"),
        ("tech", "tech"),
        ("computer", "tech"),
        ("programming", "tech"),
        ("coding", "tech"),
        ("animal", "animals"),
        ("nature", "animals"),
        ("game", "games"),
        ("gaming", "games"),
        ("chess", "games"),
        ("music", "music"),
    ];
    ALIASES
        .iter()
        .find(|(alias, _)| t.contains(alias) || (t.len() >= 3 && alias.contains(t.as_str())))
        .map(|(_, key)| *key)
}
/// Question indices for a topic, and a note when the topic had none.
fn trivia_pool(topic: Option<&str>) -> (Vec<usize>, String) {
    let all = || (0..TRIVIA.len()).collect::<Vec<_>>();
    let Some(topic) = topic.map(str::trim).filter(|t| !t.is_empty()) else {
        return (all(), String::new());
    };
    match topic_key(topic) {
        Some(key) => (
            (0..TRIVIA.len())
                .filter(|&i| TRIVIA[i].topic == key)
                .collect(),
            String::new(),
        ),
        None => (
            all(),
            format!(
                "No questions about \"{}\"; here's a general one. ",
                one_line(topic, 40)
            ),
        ),
    }
}
fn ask_text(question: &Question) -> String {
    let mut text = question.text.to_string();
    for (letter, choice) in LETTERS.iter().zip(question.choices) {
        text.push_str(&format!("\n{letter}) {choice}"));
    }
    text
}
fn normalize_answer(text: &str) -> String {
    let lower: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let words = match words.as_slice() {
        ["the", rest @ ..] if !rest.is_empty() => rest,
        all => all,
    };
    words.join(" ")
}
/// Some(right?) for an answer, or None when it isn't recognisable as one.
pub fn trivia_answer(question: &Question, answer: &str) -> Option<bool> {
    let answer = normalize_answer(answer);
    if answer.is_empty() {
        return None;
    }
    let choices: Vec<String> = question
        .choices
        .iter()
        .map(|c| normalize_answer(c))
        .collect();
    if let Some(i) = choices.iter().position(|c| *c == answer) {
        return Some(i == question.answer);
    }
    if answer.len() == 1
        && let Some(i) = LETTERS.iter().position(|l| l.eq_ignore_ascii_case(&answer))
    {
        return Some(i == question.answer);
    }
    // "it's 8 legs", "option b": one choice (or letter) named as a whole word.
    let padded = format!(" {answer} ");
    let named: Vec<usize> = (0..choices.len())
        .filter(|&i| padded.contains(&format!(" {} ", choices[i])))
        .collect();
    if let [i] = named.as_slice() {
        return Some(*i == question.answer);
    }
    if named.len() > 1 {
        return None;
    }
    let letters: Vec<usize> = (0..LETTERS.len())
        .filter(|&i| {
            let l = LETTERS[i].to_lowercase();
            padded.contains(&format!(" option {l} ")) || padded.contains(&format!(" answer {l} "))
        })
        .collect();
    if let [i] = letters.as_slice() {
        return Some(*i == question.answer);
    }
    Some(false)
}

struct ActiveTrivia {
    index: usize,
    started: Instant,
    tried: HashSet<u64>,
}
struct ActiveGuess {
    secret: u32,
    tries: u32,
    last: Instant,
}
#[derive(Default)]
struct ChannelGames {
    trivia: Option<ActiveTrivia>,
    last_trivia: Option<usize>,
    guess: Option<ActiveGuess>,
    scores: HashMap<u64, u32>,
    touched: Option<Instant>,
}
impl ChannelGames {
    fn award(&mut self, user: u64) -> u32 {
        let score = self.scores.entry(user).or_default();
        *score = score.saturating_add(1);
        *score
    }
}

/// Game state for one bot: per channel, with scores per user. Kept in memory only.
pub struct GameStore {
    channels: Mutex<HashMap<u64, ChannelGames>>,
    rng: Mutex<Rng>,
}
impl Default for GameStore {
    fn default() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            rng: Mutex::new(Rng::seeded()),
        }
    }
}
impl GameStore {
    #[cfg(test)]
    fn with_seed(seed: u64) -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
            rng: Mutex::new(Rng::new(seed)),
        }
    }
    fn with_rng<T>(&self, f: impl FnOnce(&mut Rng) -> T) -> T {
        let mut rng = self.rng.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut rng)
    }
    /// Lock order: `channels`, then `rng`.
    fn with_channel<T>(
        &self,
        channel: u64,
        now: Instant,
        f: impl FnOnce(&mut ChannelGames, &mut Rng) -> T,
    ) -> T {
        let mut map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(&channel) && map.len() >= MAX_GAME_CHANNELS {
            let oldest = map.iter().min_by_key(|(_, g)| g.touched).map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        let games = map.entry(channel).or_default();
        games.touched = Some(now);
        let mut rng = self.rng.lock().unwrap_or_else(|e| e.into_inner());
        f(games, &mut rng)
    }
    /// A user's points in a channel.
    #[cfg(test)]
    fn score(&self, channel: u64, user: u64) -> u32 {
        let map = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&channel)
            .and_then(|g| g.scores.get(&user))
            .copied()
            .unwrap_or(0)
    }

    pub fn start_trivia(&self, channel: u64, topic: Option<&str>, now: Instant) -> String {
        self.with_channel(channel, now, |g, rng| {
            if let Some(open) = &g.trivia
                && now.duration_since(open.started) < TRIVIA_TIME
            {
                return format!(
                    "A trivia question is already open. Repeat it:\n{}",
                    ask_text(&TRIVIA[open.index])
                );
            }
            let (pool, note) = trivia_pool(topic);
            let mut at = rng.below(pool.len() as u64) as usize;
            if pool.len() > 1 && Some(pool[at]) == g.last_trivia {
                at = (at + 1) % pool.len();
            }
            let index = pool[at];
            g.trivia = Some(ActiveTrivia {
                index,
                started: now,
                tried: HashSet::new(),
            });
            g.last_trivia = Some(index);
            format!(
                "{note}Post this trivia question exactly and don't reveal the answer. Players answer within {} seconds (one try each):\n{}",
                TRIVIA_TIME.as_secs(),
                ask_text(&TRIVIA[index])
            )
        })
    }

    pub fn answer_trivia(
        &self,
        channel: u64,
        user: u64,
        who: &str,
        answer: &str,
        now: Instant,
    ) -> String {
        self.with_channel(channel, now, |g, _| {
            let Some(open) = g.trivia.as_mut() else {
                return "No trivia question is open. Start one with start_trivia.".into();
            };
            let question = &TRIVIA[open.index];
            let right = question.choices[question.answer];
            if now.duration_since(open.started) >= TRIVIA_TIME {
                g.trivia = None;
                return format!("Time's up for that question. The answer was {right}.");
            }
            if open.tried.contains(&user) {
                return format!("{who} already answered this question.");
            }
            match trivia_answer(question, answer) {
                None => "Answer with A, B, C, D or the answer text.".into(),
                Some(false) => {
                    open.tried.insert(user);
                    format!("Not quite, {who}. Others can still answer.")
                }
                Some(true) => {
                    g.trivia = None;
                    let score = g.award(user);
                    format!("Correct, {who}! The answer is {right}. {who} has {score} point(s) in this channel.")
                }
            }
        })
    }

    pub fn guess_number(
        &self,
        channel: u64,
        user: u64,
        who: &str,
        guess: i64,
        now: Instant,
    ) -> Result<String, String> {
        let guess = u32::try_from(guess)
            .ok()
            .filter(|g| (1..=GUESS_MAX).contains(g))
            .ok_or_else(|| format!("Guess a whole number from 1 to {GUESS_MAX}."))?;
        Ok(self.with_channel(channel, now, |g, rng| {
            if g.guess.as_ref().is_some_and(|x| now.duration_since(x.last) >= GUESS_IDLE) {
                g.guess = None;
            }
            let fresh = g.guess.is_none();
            let game = g.guess.get_or_insert_with(|| ActiveGuess {
                secret: 1 + rng.below(u64::from(GUESS_MAX)) as u32,
                tries: 0,
                last: now,
            });
            game.tries += 1;
            game.last = now;
            let (secret, tries) = (game.secret, game.tries);
            let start = if fresh {
                format!("New game: I picked a number from 1 to {GUESS_MAX}. ")
            } else {
                String::new()
            };
            match guess_hint(secret, guess) {
                CmpOrdering::Greater => format!("{start}{guess} is too low. Go higher."),
                CmpOrdering::Less => format!("{start}{guess} is too high. Go lower."),
                CmpOrdering::Equal => {
                    g.guess = None;
                    let score = g.award(user);
                    format!("{start}{who} got it: {secret}! Found in {tries} guess(es) by the channel. {who} has {score} point(s) in this channel.")
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, rank: Rank, perms: Permissions) -> Standing {
        Standing {
            id,
            perms,
            guild_perms: perms,
            rank,
            owner: false,
        }
    }
    fn mods() -> Permissions {
        Permissions::KICK_MEMBERS
            | Permissions::BAN_MEMBERS
            | Permissions::MANAGE_MESSAGES
            | Permissions::MODERATE_MEMBERS
            | Permissions::MANAGE_ROLES
    }

    #[test]
    fn moderation_needs_setting_and_both_permissions() {
        let requester = member(1, role_rank(5, 10), mods());
        let bot = member(2, role_rank(9, 11), mods());
        let target = member(3, role_rank(1, 12), Permissions::empty());
        assert!(check_moderation(ModAction::Kick, true, &requester, &bot, Some(&target)).is_ok());
        let off = check_moderation(ModAction::Kick, false, &requester, &bot, Some(&target));
        assert!(off.unwrap_err().contains("settings"));
        let weak = member(1, role_rank(5, 10), Permissions::SEND_MESSAGES);
        let err = check_moderation(ModAction::Ban, true, &weak, &bot, Some(&target)).unwrap_err();
        assert!(err.starts_with("You need the Ban Members"));
        let weak_bot = member(2, role_rank(9, 11), Permissions::KICK_MEMBERS);
        let err = check_moderation(ModAction::Ban, true, &requester, &weak_bot, Some(&target))
            .unwrap_err();
        assert!(err.starts_with("I need the Ban Members"));
        // Administrator implies every permission.
        let admin = member(1, role_rank(5, 10), Permissions::ADMINISTRATOR);
        assert!(check_moderation(ModAction::Timeout, true, &admin, &bot, Some(&target)).is_ok());
        // Deleting without a target only checks permissions.
        assert!(check_moderation(ModAction::Delete, true, &requester, &bot, None).is_ok());
    }

    #[test]
    fn moderation_respects_role_hierarchy_and_protected_targets() {
        let requester = member(1, role_rank(5, 10), mods());
        let bot = member(2, role_rank(9, 11), mods());
        let equal = member(3, role_rank(5, 10), Permissions::empty());
        assert!(check_moderation(ModAction::Kick, true, &requester, &bot, Some(&equal)).is_err());
        let above_bot = member(3, role_rank(9, 5), Permissions::empty());
        let owner_req = Standing {
            owner: true,
            ..requester
        };
        let err = check_moderation(ModAction::Kick, true, &owner_req, &bot, Some(&above_bot))
            .unwrap_err();
        assert!(err.contains("above mine"));
        // Same position: the lower ID ranks higher.
        assert!(role_rank(5, 10) > role_rank(5, 11));
        let tie_lower = member(3, role_rank(5, 99), Permissions::empty());
        assert!(
            check_moderation(ModAction::Kick, true, &requester, &bot, Some(&tie_lower)).is_ok()
        );
        let me = member(1, role_rank(0, 1), Permissions::empty());
        assert!(
            check_moderation(ModAction::Kick, true, &requester, &bot, Some(&me))
                .unwrap_err()
                .contains("yourself")
        );
        let bot_target = member(2, role_rank(0, 1), Permissions::empty());
        assert!(
            check_moderation(ModAction::Kick, true, &requester, &bot, Some(&bot_target))
                .unwrap_err()
                .contains("myself")
        );
        let owner = Standing {
            owner: true,
            ..member(4, (0, 0), Permissions::empty())
        };
        assert!(
            check_moderation(ModAction::Ban, true, &requester, &bot, Some(&owner))
                .unwrap_err()
                .contains("owner")
        );
        // A member with no roles ranks below anyone with a role.
        assert_eq!(top_rank([]), (0, 0));
        assert_eq!(
            top_rank([role_rank(2, 7), role_rank(4, 8)]),
            role_rank(4, 8)
        );
    }

    #[test]
    fn role_changes_stay_below_both_and_never_escalate() {
        let requester = member(
            1,
            role_rank(5, 10),
            Permissions::MANAGE_ROLES | Permissions::KICK_MEMBERS,
        );
        let bot = member(2, role_rank(9, 11), Permissions::ADMINISTRATOR);
        let low = role_rank(2, 50);
        assert!(check_role_change(low, Permissions::KICK_MEMBERS, false, &requester, &bot).is_ok());
        assert!(
            check_role_change(low, Permissions::BAN_MEMBERS, false, &requester, &bot)
                .unwrap_err()
                .contains("permissions")
        );
        assert!(
            check_role_change(
                role_rank(5, 10),
                Permissions::empty(),
                false,
                &requester,
                &bot
            )
            .is_err()
        );
        assert!(check_role_change(low, Permissions::empty(), true, &requester, &bot).is_err());
        let owner = Standing {
            owner: true,
            ..requester
        };
        assert!(
            check_role_change(
                role_rank(8, 3),
                Permissions::ADMINISTRATOR,
                false,
                &owner,
                &bot
            )
            .is_ok()
        );
        assert!(
            check_role_change(role_rank(9, 3), Permissions::empty(), false, &owner, &bot)
                .unwrap_err()
                .contains("my top role")
        );
    }

    #[test]
    fn channel_overwrites_do_not_grant_server_wide_actions() {
        // Manage Roles (and Manage Messages) granted only by a channel overwrite.
        let requester = Standing {
            guild_perms: Permissions::SEND_MESSAGES,
            ..member(
                1,
                role_rank(5, 10),
                Permissions::MANAGE_ROLES | Permissions::MANAGE_MESSAGES,
            )
        };
        let bot = member(2, role_rank(9, 11), Permissions::ADMINISTRATOR);
        let target = member(3, role_rank(1, 12), Permissions::empty());
        for action in [ModAction::AddRole, ModAction::RemoveRole] {
            let err = check_moderation(action, true, &requester, &bot, Some(&target)).unwrap_err();
            assert!(err.starts_with("You need the"), "{err}");
        }
        assert!(
            check_role_change(
                role_rank(2, 50),
                Permissions::MANAGE_MESSAGES,
                false,
                &requester,
                &bot
            )
            .unwrap_err()
            .contains("permissions")
        );
        // Deleting messages is per channel, so the overwrite counts there.
        assert!(check_moderation(ModAction::Delete, true, &requester, &bot, None).is_ok());
    }

    #[test]
    fn confirmation_rules() {
        assert!(!needs_confirmation(ModAction::Delete, 5));
        assert!(needs_confirmation(ModAction::Delete, 6));
        for a in [
            ModAction::Timeout,
            ModAction::Kick,
            ModAction::Ban,
            ModAction::AddRole,
            ModAction::RemoveRole,
        ] {
            assert!(needs_confirmation(a, 0));
        }
        assert_eq!(ModAction::from_tool("ban_member"), Some(ModAction::Ban));
        assert_eq!(ModAction::from_tool("roll_dice"), None);
    }

    #[test]
    fn dj_role_by_name_id_or_admin() {
        let roles = vec![(42, "DJ".to_string())];
        assert!(dj_allowed("", &[], Permissions::empty()));
        assert!(dj_allowed("dj", &roles, Permissions::empty()));
        assert!(dj_allowed("<@&42>", &roles, Permissions::empty()));
        assert!(dj_allowed("42", &roles, Permissions::empty()));
        assert!(!dj_allowed("Music", &roles, Permissions::empty()));
        assert!(dj_allowed("Music", &[], Permissions::MANAGE_GUILD));
    }

    #[test]
    fn user_and_role_references() {
        let id = 123_456_789_012_345_678;
        assert_eq!(
            parse_user_ref("<@123456789012345678>"),
            Some(UserRef::Id(id))
        );
        assert_eq!(
            parse_user_ref("<@!123456789012345678>"),
            Some(UserRef::Id(id))
        );
        assert_eq!(
            parse_user_ref(" 123456789012345678 "),
            Some(UserRef::Id(id))
        );
        assert_eq!(parse_user_ref("@Kit"), Some(UserRef::Name("Kit".into())));
        assert_eq!(
            parse_user_ref("kit#1234"),
            Some(UserRef::Name("kit".into()))
        );
        assert_eq!(parse_user_ref("1234"), Some(UserRef::Name("1234".into())));
        assert_eq!(parse_user_ref("<@0>"), None);
        assert_eq!(parse_user_ref("  "), None);
        assert_eq!(parse_user_ref(&"x".repeat(65)), None);
        let candidates = vec![
            (1, vec!["kit".to_string(), "Kitty".to_string()]),
            (2, vec!["ana".to_string()]),
            (3, vec!["bo".to_string(), "Ana".to_string()]),
        ];
        assert_eq!(pick_member(&candidates, "KITTY"), Ok(1));
        assert!(
            pick_member(&candidates, "ana")
                .unwrap_err()
                .contains("Several")
        );
        assert!(
            pick_member(&candidates, "zed")
                .unwrap_err()
                .contains("No member")
        );
        let roles = vec![
            (500_000_000_000_000_001, "Mods".to_string()),
            (7, "VIP".to_string()),
        ];
        assert_eq!(pick_role(&roles, "<@&7>"), Ok(7));
        assert_eq!(pick_role(&roles, "@vip"), Ok(7));
        assert_eq!(
            pick_role(&roles, "500000000000000001"),
            Ok(500_000_000_000_000_001)
        );
        assert!(pick_role(&roles, "<@&8>").is_err());
        assert!(pick_role(&roles, "").is_err());
        assert_eq!(parse_channel("<#99>"), Some(99));
        assert_eq!(parse_channel(" 99 "), Some(99));
        assert_eq!(parse_channel(""), None);
    }

    #[test]
    fn arguments_are_normalized_and_cleaned() {
        assert_eq!(normalize_args(&json!("{\"count\":3}"))["count"], 3);
        assert_eq!(normalize_args(&json!(null)), json!({}));
        assert_eq!(normalize_args(&json!("nope")), json!({}));
        let args = json!({"count":"7","f":2.0,"neg":-1,"opts":"a | b|c","csv":"x, y"});
        assert_eq!(arg_u64(&args, "count"), Some(7));
        assert_eq!(arg_u64(&args, "f"), Some(2));
        assert_eq!(arg_u64(&args, "neg"), None);
        assert_eq!(list_arg(&args, "opts").len(), 3);
        assert_eq!(list_arg(&args, "csv"), vec!["x", " y"]);
        assert_eq!(clean_reason(Some(" spam\nlinks ")), "spam links");
        assert_eq!(clean_reason(None), "No reason given");
        assert_eq!(clean_name("\u{7}"), "Someone");
        assert_eq!(clean_reason(Some(&"r".repeat(500))).len(), MAX_REASON);
    }

    #[test]
    fn games_list_filters_tools() {
        assert!(game_enabled(&[], "dice"));
        assert!(game_enabled(&["".into()], "dice"));
        assert!(game_enabled(&["Trivia".into()], "trivia"));
        assert!(!game_enabled(&["trivia".into()], "dice"));
        assert_eq!(game_of("answer_trivia"), Some("trivia"));
        assert_eq!(game_of("kick_member"), None);
        // Every tool has a definition with its own name, and every game id is listed.
        let defs = definitions();
        for (name, def) in &defs {
            assert_eq!(def["function"]["name"], *name);
            assert!(
                MUSIC_TOOLS.contains(name)
                    || ModAction::from_tool(name).is_some()
                    || game_of(name).is_some(),
                "{name}"
            );
        }
        assert_eq!(defs.len(), 19);
        for (name, _) in &defs {
            if let Some(game) = game_of(name) {
                assert!(GAMES.iter().any(|(id, _)| *id == game));
            }
        }
    }

    #[test]
    fn dice_coin_and_rps() {
        assert_eq!(validate_dice(None, None), Ok((6, 1)));
        assert!(validate_dice(Some(1), None).is_err());
        assert!(validate_dice(Some(6), Some(21)).is_err());
        let mut rng = Rng::new(7);
        let rolls = roll(&mut rng, 6, 20);
        assert_eq!(rolls.len(), 20);
        assert!(rolls.iter().all(|r| (1..=6).contains(r)));
        assert_eq!(Rng::new(3).next_u64(), Rng::new(3).next_u64());
        assert_eq!(rng.below(0), 0);
        assert_eq!(parse_rps(" Rock! "), Some(Rps::Rock));
        assert_eq!(parse_rps("s"), Some(Rps::Scissors));
        assert_eq!(parse_rps("lizard"), None);
        assert_eq!(rps_result(Rps::Rock, Rps::Scissors), Outcome::Win);
        assert_eq!(rps_result(Rps::Rock, Rps::Paper), Outcome::Lose);
        assert_eq!(rps_result(Rps::Paper, Rps::Paper), Outcome::Draw);
        assert_eq!(rps_result(Rps::Scissors, Rps::Paper), Outcome::Win);
    }

    #[test]
    fn polls_are_validated() {
        fn opts(v: &[&str]) -> Vec<String> {
            v.iter().map(|s| s.to_string()).collect()
        }
        let (q, o) = validate_poll(" Lunch?\n", &opts(&["Pizza", " pizza ", "", "Tacos"])).unwrap();
        assert_eq!((q.as_str(), o), ("Lunch?", opts(&["Pizza", "Tacos"])));
        assert!(validate_poll("", &opts(&["a", "b"])).is_err());
        assert!(validate_poll("q", &opts(&["a"])).is_err());
        let eleven: Vec<String> = (0..11).map(|i| i.to_string()).collect();
        assert!(validate_poll("q", &eleven).is_err());
        assert!(validate_poll("q", &opts(&["a", "b".repeat(101).as_str()])).is_err());
        assert!(validate_poll(&"q".repeat(301), &opts(&["a", "b"])).is_err());
    }

    #[test]
    fn trivia_answers_by_letter_text_or_word() {
        let question = &TRIVIA[23]; // spider legs: B) 8
        assert_eq!(question.choices[question.answer], "8");
        assert_eq!(trivia_answer(question, "b"), Some(true));
        assert_eq!(trivia_answer(question, "B)"), Some(true));
        assert_eq!(trivia_answer(question, "8"), Some(true));
        assert_eq!(trivia_answer(question, "it has 8 legs"), Some(true));
        assert_eq!(trivia_answer(question, "6"), Some(false));
        assert_eq!(trivia_answer(question, "option b"), Some(true));
        assert_eq!(trivia_answer(question, "6 or 8"), None);
        assert_eq!(trivia_answer(question, "  ?! "), None);
        let sun = &TRIVIA[7];
        assert_eq!(trivia_answer(sun, "the sun"), Some(true));
        assert_eq!(trivia_answer(sun, "Sun"), Some(true));
        for q in TRIVIA {
            assert!(q.answer < 4 && !q.text.is_empty());
            assert_eq!(
                trivia_answer(q, q.choices[q.answer]),
                Some(true),
                "{}",
                q.text
            );
        }
    }

    #[test]
    fn trivia_topics_and_state() {
        assert_eq!(topic_key("Astronomy"), Some("space"));
        assert_eq!(topic_key("video games"), Some("games"));
        assert_eq!(topic_key("cooking"), None);
        let (pool, note) = trivia_pool(Some("cooking"));
        assert_eq!((pool.len(), note.is_empty()), (TRIVIA.len(), false));
        assert!(
            trivia_pool(Some("space"))
                .0
                .iter()
                .all(|&i| TRIVIA[i].topic == "space")
        );

        let store = GameStore::with_seed(1);
        let now = Instant::now();
        assert!(
            store
                .answer_trivia(5, 1, "A", "b", now)
                .contains("No trivia")
        );
        let asked = store.start_trivia(5, Some("animals"), now);
        assert!(asked.contains("A) "));
        assert!(store.start_trivia(5, None, now).contains("already open"));
        let index = store.channels.lock().unwrap()[&5]
            .trivia
            .as_ref()
            .unwrap()
            .index;
        let question = &TRIVIA[index];
        let wrong = LETTERS[(question.answer + 1) % 4];
        let right = LETTERS[question.answer];
        assert!(
            store
                .answer_trivia(5, 1, "A", wrong, now)
                .contains("Not quite")
        );
        assert!(
            store
                .answer_trivia(5, 1, "A", right, now)
                .contains("already answered")
        );
        assert!(
            store
                .answer_trivia(5, 2, "B", "???", now)
                .contains("Answer with")
        );
        assert!(
            store
                .answer_trivia(5, 2, "B", right, now)
                .contains("Correct")
        );
        assert_eq!(
            (store.score(5, 2), store.score(5, 1), store.score(6, 2)),
            (1, 0, 0)
        );
        // An expired question reveals its answer and closes.
        store.start_trivia(5, None, now);
        let late = now + TRIVIA_TIME;
        assert!(
            store
                .answer_trivia(5, 3, "C", "a", late)
                .contains("Time's up")
        );
        assert!(
            store
                .answer_trivia(5, 3, "C", "a", late)
                .contains("No trivia")
        );
    }

    #[test]
    fn number_guessing_finds_the_secret() {
        assert_eq!(guess_hint(50, 20), CmpOrdering::Greater);
        assert_eq!(guess_hint(50, 70), CmpOrdering::Less);
        let store = GameStore::with_seed(9);
        let now = Instant::now();
        assert!(store.guess_number(1, 1, "A", 0, now).is_err());
        assert!(store.guess_number(1, 1, "A", 101, now).is_err());
        // Binary search always wins within 7 guesses.
        let (mut low, mut high) = (1, GUESS_MAX as i64);
        let mut result = String::new();
        for _ in 0..7 {
            let guess = (low + high) / 2;
            result = store.guess_number(1, 7, "Kit", guess, now).unwrap();
            if result.contains("too low") {
                low = guess + 1;
            } else if result.contains("too high") {
                high = guess - 1;
            } else {
                break;
            }
        }
        assert!(result.contains("got it"), "{result}");
        assert_eq!(store.score(1, 7), 1);
        // The next guess starts a new game; a long pause starts one too.
        assert!(
            store
                .guess_number(1, 7, "Kit", 50, now)
                .unwrap()
                .starts_with("New game")
        );
        let r = store
            .guess_number(1, 7, "Kit", 1, now + GUESS_IDLE)
            .unwrap();
        assert!(r.starts_with("New game"), "{r}");
    }

    #[test]
    fn game_state_is_capped_per_channel() {
        let store = GameStore::with_seed(2);
        let now = Instant::now();
        for channel in 0..(MAX_GAME_CHANNELS as u64 + 5) {
            store.start_trivia(channel, None, now + Duration::from_millis(channel));
        }
        let map = store.channels.lock().unwrap();
        assert_eq!(map.len(), MAX_GAME_CHANNELS);
        assert!(!map.contains_key(&0));
    }
}

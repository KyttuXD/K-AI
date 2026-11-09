//! Voice music for Discord bots: songbird playback with per-guild queues, tracks
//! resolved through a pinned yt-dlp build, per-user and duration caps, DJ-role
//! rules and leaving voice after 5 idle minutes.
//!
//! One `Music` belongs to one bot: register its songbird instance on that bot's
//! serenity client builder (`register_songbird_with(music.songbird())`), and only
//! when the bot's music tool is on.

use crate::core::{config::data_dir, llama, setup};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde_json::Value;
use serenity::all::{ChannelId, Context, GuildId, Permissions, UserId};
use songbird::{
    Songbird,
    input::{HttpRequest, Input},
    tracks::Track,
};
use std::{
    collections::HashMap,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
    time::{Duration, Instant},
};

/// The yt-dlp release that is downloaded on first use, and the SHA-256 of each asset.
pub const YTDLP_VERSION: &str = "2026.10.26";
const YTDLP_ASSETS: &[(&str, &str, &str, &str)] = &[
    (
        "windows",
        "x86_64",
        "yt-dlp.exe",
        "f5389d3698ecde4ef06f4e6b1ce0294fe9ac1396228c8990b8d3416d6d177a7d",
    ),
    (
        "macos",
        "*",
        "yt-dlp_macos",
        "0f922c15ccc0b83eefa1c8bc9104e434318c4bf16e408e1ee93b7745a6efe0aa",
    ),
    (
        "linux",
        "x86_64",
        "yt-dlp_linux",
        "767ba9dce7052aa196f8b12652f433d76de128ba168966fa84b6394d5e6c6795",
    ),
    (
        "linux",
        "aarch64",
        "yt-dlp_linux_aarch64",
        "e87e9533e89fe5889abaa21f5114dfe7756d6aa6219b83551add04d48b18e483",
    ),
];

/// Shown once in the UI when music is first enabled.
pub const TOS_NOTE: &str = "Streaming from YouTube may break YouTube's terms of service; that is the bot owner's responsibility.";

/// Formats songbird/symphonia decode without ffmpeg: Opus (WebM), then AAC (M4A).
const FORMAT: &str = "bestaudio[acodec=opus]/bestaudio[ext=m4a]/bestaudio";
/// Leave voice after this long with an empty queue.
pub const IDLE_LEAVE: Duration = Duration::from_secs(5 * 60);
const IDLE_POLL: Duration = Duration::from_secs(30);
/// Tracks one guild may queue in total, whatever the per-user cap.
const MAX_QUEUE_TOTAL: usize = 50;
const MAX_QUERY_CHARS: usize = 200;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
const QUEUE_SHOWN: usize = 10;

/// Limits from the bot's music settings (BotProfile.tools.music_tools).
#[derive(Clone, Debug)]
pub struct MusicLimits {
    /// Role ID, `<@&id>` mention or role name; empty = everyone may skip/stop.
    pub dj_role: String,
    pub max_queue_per_user: usize,
    pub max_track_minutes: u32,
}
impl Default for MusicLimits {
    fn default() -> Self {
        Self {
            dj_role: String::new(),
            max_queue_per_user: 5,
            max_track_minutes: 15,
        }
    }
}

/// Who asked: their roles decide DJ-only actions.
#[derive(Clone, Debug, Default)]
pub struct Requester {
    pub user: u64,
    pub name: String,
    /// (role id, role name) of the member's roles.
    pub roles: Vec<(u64, String)>,
    /// Administrator, Manage Server or guild owner: always counts as DJ.
    pub admin: bool,
}
impl Requester {
    /// Builds a requester from Discord. Roles are only fetched when a DJ role is set.
    pub async fn fetch(
        ctx: &Context,
        guild: GuildId,
        user: UserId,
        name: &str,
        limits: &MusicLimits,
    ) -> Requester {
        let mut out = Requester {
            user: user.get(),
            name: name.to_string(),
            roles: vec![],
            admin: false,
        };
        if limits.dj_role.trim().is_empty() {
            return out;
        }
        let partial = ctx.http.get_guild(guild).await;
        let member = ctx.http.get_member(guild, user).await;
        let roles = ctx.http.get_guild_roles(guild).await;
        if let Ok(g) = partial {
            out.admin = g.owner_id == user;
        }
        if let (Ok(member), Ok(roles)) = (member, roles) {
            let admin_bits = Permissions::ADMINISTRATOR | Permissions::MANAGE_GUILD;
            for role in roles.iter().filter(|r| member.roles.contains(&r.id)) {
                out.admin |= role.permissions.intersects(admin_bits);
                out.roles.push((role.id.get(), role.name.clone()));
            }
        }
        out
    }
}

/// A user's current voice channel in `guild` (no cache needed), if any.
pub async fn user_voice_channel(ctx: &Context, guild: GuildId, user: UserId) -> Option<ChannelId> {
    ctx.http
        .get_user_voice_state(guild, user)
        .await
        .ok()
        .and_then(|state| state.channel_id)
}

/// Data attached to each queued songbird track.
#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub title: String,
    #[allow(dead_code)]
    pub page_url: String,
    pub seconds: u64,
    pub requester: u64,
    pub requester_name: String,
}

#[derive(Default)]
struct GuildState {
    /// The idle watcher for this guild is running.
    watching: bool,
    /// Tracks being resolved per user (counted against the per-user cap).
    pending: HashMap<u64, usize>,
}

/// One bot's music: its songbird instance and per-guild queue state.
pub struct Music {
    voice: Arc<Songbird>,
    http: reqwest::Client,
    guilds: Mutex<HashMap<u64, GuildState>>,
}

/// Releases a pending per-user reservation when a play attempt ends.
struct Reservation {
    music: Arc<Music>,
    guild: u64,
    user: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut guilds = self.music.guilds.lock().unwrap();
        if let Some(state) = guilds.get_mut(&self.guild)
            && let Some(n) = state.pending.get_mut(&self.user)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.pending.remove(&self.user);
            }
        }
    }
}

/// Music actions that may need the DJ role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Skip,
    Stop,
    Pause,
    Resume,
    Leave,
}

impl Music {
    pub fn new() -> Arc<Self> {
        let http = reqwest::Client::builder()
            .user_agent("kyttu-ai")
            .connect_timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Arc::new(Self {
            voice: Songbird::serenity(),
            http,
            guilds: Mutex::new(HashMap::new()),
        })
    }

    /// The songbird instance to register on this bot's serenity client builder.
    pub fn songbird(&self) -> Arc<Songbird> {
        Arc::clone(&self.voice)
    }

    /// Resolves `query` (search words or an http(s) URL), joins the requester's voice
    /// channel and queues the track. `voice_channel` None looks it up over HTTP.
    /// Returns the reply text.
    pub async fn play(
        self: &Arc<Self>,
        ctx: &Context,
        guild: GuildId,
        voice_channel: Option<ChannelId>,
        requester: &Requester,
        query: &str,
        limits: &MusicLimits,
    ) -> Result<String> {
        let target = ytdlp_target(query)?;
        let channel = match voice_channel {
            Some(c) => Some(c),
            None => user_voice_channel(ctx, guild, UserId::new(requester.user.max(1))).await,
        };
        let channel =
            channel.ok_or_else(|| anyhow!("Join a voice channel first, then ask again."))?;

        let _reservation = self.reserve(guild, requester.user, limits).await?;
        let resolved = resolve(&ensure_ytdlp().await?, &target, limits.max_track_minutes).await?;

        let call = self.join(guild, channel).await?;
        let info = TrackInfo {
            title: resolved.title.clone(),
            page_url: resolved.page_url.clone(),
            seconds: resolved.seconds,
            requester: requester.user,
            requester_name: requester.name.clone(),
        };
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in &resolved.headers {
            if let (Ok(k), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                reqwest::header::HeaderValue::from_str(v),
            ) {
                headers.insert(k, v);
            }
        }
        let mut request =
            HttpRequest::new_with_headers(self.http.clone(), resolved.stream_url, headers);
        request.content_length = resolved.filesize;
        let track = Track::new_with_data(Input::from(request), Arc::new(info));
        let preload = Duration::from_secs(resolved.seconds).checked_sub(Duration::from_secs(5));
        let position = {
            let mut handler = call.lock().await;
            // Re-check under the lock: other plays may have finished meanwhile.
            let queued: Vec<u64> = queued_requesters(&handler.queue().current_queue());
            check_queue(&queued, 0, requester.user, limits.max_queue_per_user)?;
            handler.enqueue_with_preload(track, preload);
            handler.queue().len()
        };
        self.watch_idle(guild);
        let length = clock(resolved.seconds);
        Ok(if position <= 1 {
            format!("Now playing: **{}** ({length})", escape(&resolved.title))
        } else {
            format!(
                "Queued #{position}: **{}** ({length})",
                escape(&resolved.title)
            )
        })
    }

    /// Skips the current track (DJ, admin, or the track's requester).
    pub async fn skip(
        &self,
        guild: GuildId,
        requester: &Requester,
        limits: &MusicLimits,
    ) -> Result<String> {
        let call = self.call(guild)?;
        let handler = call.lock().await;
        let queue = handler.queue();
        let current = queue
            .current()
            .ok_or_else(|| anyhow!("Nothing is playing."))?;
        let info = current.data::<TrackInfo>();
        ensure_dj(
            Action::Skip,
            limits,
            requester,
            info.requester == requester.user,
        )?;
        queue.skip().map_err(|e| anyhow!("Could not skip: {e}"))?;
        Ok(format!("Skipped **{}**.", escape(&info.title)))
    }

    /// Stops playback and clears the queue (DJ or admin).
    pub async fn stop(
        &self,
        guild: GuildId,
        requester: &Requester,
        limits: &MusicLimits,
    ) -> Result<String> {
        ensure_dj(Action::Stop, limits, requester, false)?;
        let call = self.call(guild)?;
        let handler = call.lock().await;
        ensure!(!handler.queue().is_empty(), "Nothing is playing.");
        handler.queue().stop();
        Ok("Stopped and cleared the queue.".into())
    }

    pub async fn pause(
        &self,
        guild: GuildId,
        requester: &Requester,
        limits: &MusicLimits,
    ) -> Result<String> {
        ensure_dj(Action::Pause, limits, requester, false)?;
        let call = self.call(guild)?;
        let handler = call.lock().await;
        ensure!(!handler.queue().is_empty(), "Nothing is playing.");
        handler
            .queue()
            .pause()
            .map_err(|e| anyhow!("Could not pause: {e}"))?;
        Ok("Paused.".into())
    }

    pub async fn resume(
        &self,
        guild: GuildId,
        requester: &Requester,
        limits: &MusicLimits,
    ) -> Result<String> {
        ensure_dj(Action::Resume, limits, requester, false)?;
        let call = self.call(guild)?;
        let handler = call.lock().await;
        ensure!(!handler.queue().is_empty(), "Nothing is queued.");
        handler
            .queue()
            .resume()
            .map_err(|e| anyhow!("Could not resume: {e}"))?;
        Ok("Resumed.".into())
    }

    /// The queue as a short message.
    pub async fn queue_text(&self, guild: GuildId) -> String {
        let Some(call) = self.voice.get(guild) else {
            return "The queue is empty.".into();
        };
        let tracks = call.lock().await.queue().current_queue();
        let infos: Vec<Arc<TrackInfo>> = tracks.iter().map(|t| t.data::<TrackInfo>()).collect();
        format_queue(&infos)
    }

    /// Clears the queue and leaves voice (DJ or admin).
    pub async fn leave(
        &self,
        guild: GuildId,
        requester: &Requester,
        limits: &MusicLimits,
    ) -> Result<String> {
        ensure_dj(Action::Leave, limits, requester, false)?;
        ensure!(
            self.voice.get(guild).is_some(),
            "I'm not in a voice channel."
        );
        self.drop_guild(guild).await;
        Ok("Left the voice channel.".into())
    }

    /// Leaves every voice channel (call before the bot disconnects).
    pub async fn shutdown(&self) {
        let guilds: Vec<u64> = self.guilds.lock().unwrap().keys().copied().collect();
        for g in guilds {
            self.drop_guild(GuildId::new(g)).await;
        }
    }

    /// Whether the bot is connected to voice in `guild`.
    #[allow(dead_code)]
    pub async fn in_voice(&self, guild: GuildId) -> bool {
        match self.voice.get(guild) {
            Some(call) => call.lock().await.current_channel().is_some(),
            None => false,
        }
    }

    fn call(&self, guild: GuildId) -> Result<Arc<tokio::sync::Mutex<songbird::Call>>> {
        self.voice
            .get(guild)
            .ok_or_else(|| anyhow!("Nothing is playing."))
    }

    async fn drop_guild(&self, guild: GuildId) {
        if let Some(call) = self.voice.get(guild) {
            call.lock().await.queue().stop();
        }
        let _ = self.voice.remove(guild).await;
        self.guilds.lock().unwrap().remove(&guild.get());
    }

    /// Checks the per-user and total caps (queued plus being resolved) and holds a slot.
    async fn reserve(
        self: &Arc<Self>,
        guild: GuildId,
        user: u64,
        limits: &MusicLimits,
    ) -> Result<Reservation> {
        let queued = match self.voice.get(guild) {
            Some(call) => queued_requesters(&call.lock().await.queue().current_queue()),
            None => vec![],
        };
        let mut guilds = self.guilds.lock().unwrap();
        let state = guilds.entry(guild.get()).or_default();
        let pending_mine = state.pending.get(&user).copied().unwrap_or(0);
        let pending_all: usize = state.pending.values().sum();
        let mut all = queued;
        all.extend(std::iter::repeat_n(
            0,
            pending_all.saturating_sub(pending_mine),
        ));
        check_queue(&all, pending_mine, user, limits.max_queue_per_user)?;
        *state.pending.entry(user).or_default() += 1;
        Ok(Reservation {
            music: Arc::clone(self),
            guild: guild.get(),
            user,
        })
    }

    /// Joins `channel`, unless the bot is busy in another channel of this guild.
    async fn join(
        &self,
        guild: GuildId,
        channel: ChannelId,
    ) -> Result<Arc<tokio::sync::Mutex<songbird::Call>>> {
        if let Some(call) = self.voice.get(guild) {
            let handler = call.lock().await;
            if let Some(current) = handler.current_channel()
                && current.0.get() != channel.get()
                && !handler.queue().is_empty()
            {
                bail!(
                    "I'm already playing in <#{}>. Join that channel to add songs.",
                    current.0.get()
                );
            }
        }
        match self.voice.join(guild, channel).await {
            Ok(call) => {
                let _ = call.lock().await.deafen(true).await;
                Ok(call)
            }
            Err(e) => {
                // songbird: leave before trying again after a failed join.
                let _ = self.voice.leave(guild).await;
                Err(anyhow!(
                    "Could not join <#{}> ({e}). Check the bot's Connect and Speak permissions.",
                    channel.get()
                ))
            }
        }
    }

    /// Leaves `guild` after IDLE_LEAVE with an empty queue, or when disconnected.
    fn watch_idle(self: &Arc<Self>, guild: GuildId) {
        {
            let mut guilds = self.guilds.lock().unwrap();
            let state = guilds.entry(guild.get()).or_default();
            if state.watching {
                return;
            }
            state.watching = true;
        }
        let music = Arc::clone(self);
        tokio::spawn(async move {
            let mut idle_since = None;
            loop {
                tokio::time::sleep(IDLE_POLL).await;
                let Some(call) = music.voice.get(guild) else {
                    break;
                };
                let (connected, empty) = {
                    let handler = call.lock().await;
                    (
                        handler.current_channel().is_some(),
                        handler.queue().is_empty(),
                    )
                };
                let (since, leave) =
                    idle_step(idle_since, connected, empty, Instant::now(), IDLE_LEAVE);
                idle_since = since;
                if leave {
                    break;
                }
            }
            music.drop_guild(guild).await;
        });
    }
}

/// Fails with a clear message when `requester` may not do `action`.
fn ensure_dj(
    action: Action,
    limits: &MusicLimits,
    requester: &Requester,
    owns_current: bool,
) -> Result<()> {
    if dj_allowed(
        action,
        &limits.dj_role,
        &requester.roles,
        requester.admin,
        owns_current,
    ) {
        return Ok(());
    }
    let what = match action {
        Action::Skip => "skip other people's tracks",
        Action::Stop => "stop the music",
        Action::Leave => "make me leave",
        Action::Pause | Action::Resume => "do that",
    };
    bail!("Only the DJ role can {what}.")
}

/// DJ rule: with a DJ role set, skip/stop/leave need it (or admin); anyone may skip
/// their own track; pause and resume are open to everyone.
pub fn dj_allowed(
    action: Action,
    dj_role: &str,
    roles: &[(u64, String)],
    admin: bool,
    owns_current: bool,
) -> bool {
    let dj_role = dj_role.trim();
    if dj_role.is_empty() || admin {
        return true;
    }
    match action {
        Action::Pause | Action::Resume => true,
        Action::Skip if owns_current => true,
        Action::Skip | Action::Stop | Action::Leave => has_role(dj_role, roles),
    }
}

fn has_role(dj_role: &str, roles: &[(u64, String)]) -> bool {
    let id = dj_role
        .trim_start_matches("<@&")
        .trim_end_matches('>')
        .parse::<u64>()
        .ok();
    let name = dj_role.trim_start_matches('@');
    roles
        .iter()
        .any(|(rid, rname)| Some(*rid) == id || rname.eq_ignore_ascii_case(name))
}

fn queued_requesters(tracks: &[songbird::tracks::TrackHandle]) -> Vec<u64> {
    tracks
        .iter()
        .map(|t| t.data::<TrackInfo>().requester)
        .collect()
}

/// Per-user cap (`queued` holds the requester of every queued track; `pending_mine`
/// counts this user's tracks still being resolved) and the guild total.
pub fn check_queue(
    queued: &[u64],
    pending_mine: usize,
    user: u64,
    max_per_user: usize,
) -> Result<()> {
    let max_per_user = max_per_user.max(1);
    ensure!(
        queued.len() + pending_mine < MAX_QUEUE_TOTAL,
        "The queue is full ({MAX_QUEUE_TOTAL} tracks)."
    );
    let mine = queued.iter().filter(|&&u| u == user).count() + pending_mine;
    ensure!(
        mine < max_per_user,
        "You already have {mine} track{} queued (limit {max_per_user}).",
        if mine == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Idle bookkeeping: returns the new idle start and whether to leave now.
fn idle_step(
    idle_since: Option<Instant>,
    connected: bool,
    empty: bool,
    now: Instant,
    limit: Duration,
) -> (Option<Instant>, bool) {
    if !connected {
        return (idle_since, true);
    }
    if !empty {
        return (None, false);
    }
    let since = idle_since.unwrap_or(now);
    (Some(since), now.duration_since(since) >= limit)
}

fn format_queue(tracks: &[Arc<TrackInfo>]) -> String {
    let Some(first) = tracks.first() else {
        return "The queue is empty.".into();
    };
    let mut out = format!(
        "Now playing: **{}** ({}), requested by {}",
        escape(&first.title),
        clock(first.seconds),
        escape(&first.requester_name)
    );
    for (i, t) in tracks.iter().enumerate().skip(1).take(QUEUE_SHOWN) {
        out.push_str(&format!(
            "\n{i}. {} ({}), {}",
            escape(&t.title),
            clock(t.seconds),
            escape(&t.requester_name)
        ));
    }
    let more = tracks.len().saturating_sub(QUEUE_SHOWN + 1);
    if more > 0 {
        out.push_str(&format!("\n...and {more} more"));
    }
    out
}

/// 3:07 or 1:02:03.
fn clock(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Neutralises Discord markdown and mentions in titles and names.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().take(120) {
        if matches!(c, '*' | '_' | '`' | '~' | '|' | '>' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
        if c == '@' {
            out.push('\u{200B}');
        }
    }
    out
}

// ---- yt-dlp ----

/// Release asset name and SHA-256 for an OS/arch pair (std::env::consts names).
pub fn ytdlp_asset(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    YTDLP_ASSETS
        .iter()
        .find(|(o, a, _, _)| *o == os && (*a == "*" || *a == arch))
        .map(|&(_, _, name, sha)| (name, sha))
}

/// (major, minor) from `sw_vers -productVersion` output such as "10.14.6" or "14".
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn macos_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |m| m.parse().ok())?;
    Some((major, minor))
}

pub fn ytdlp_dir() -> PathBuf {
    data_dir().join("tools").join("yt-dlp").join(YTDLP_VERSION)
}

/// The pinned yt-dlp, downloaded and verified on first use.
pub async fn ensure_ytdlp() -> Result<PathBuf> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let (asset, sha) =
        ytdlp_asset(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            anyhow!(
                "Music needs yt-dlp, which has no pinned build for {} {}.",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    // yt-dlp_macos needs macOS 10.15+, but this app's x64 build runs on 10.13.
    #[cfg(target_os = "macos")]
    {
        let version = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        if version
            .as_deref()
            .and_then(macos_version)
            .is_some_and(|v| v < (10, 15))
        {
            bail!("Music needs macOS 10.15 or later (yt-dlp).");
        }
    }
    let dest = ytdlp_dir().join(asset);
    let _guard = LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    // Only verified downloads get the final name.
    if dest.is_file() {
        return Ok(dest);
    }
    let url = format!("https://github.com/yt-dlp/yt-dlp/releases/download/{YTDLP_VERSION}/{asset}");
    let target = dest.clone();
    tokio::task::spawn_blocking(move || {
        let progress = Mutex::new(setup::SetupProgress::default());
        let cancel = AtomicBool::new(false);
        setup::download_verified(&url, 0, Some(sha), &target, &progress, &cancel)
    })
    .await
    .map_err(|e| anyhow!("yt-dlp download task failed: {e}"))?
    .context("Could not download yt-dlp")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .context("Could not make yt-dlp executable")?;
    }
    Ok(dest)
}

/// The only hosts a link may point to.
const YOUTUBE_HOSTS: &[&str] = &[
    "youtube.com",
    "www.youtube.com",
    "m.youtube.com",
    "music.youtube.com",
    "youtu.be",
];

/// The yt-dlp target for a query: an http(s) YouTube link, or "ytsearch1:<words>".
pub fn ytdlp_target(query: &str) -> Result<String> {
    let query = query.trim();
    ensure!(
        !query.is_empty(),
        "Tell me what to play: a song name or a link."
    );
    ensure!(
        query.chars().count() <= MAX_QUERY_CHARS,
        "That search is too long (max {MAX_QUERY_CHARS} characters)."
    );
    let query = query.trim_start_matches('<').trim_end_matches('>');
    let lower = query.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        let url =
            reqwest::Url::parse(query).map_err(|_| anyhow!("That link is not a valid URL."))?;
        let host = url
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_ascii_lowercase();
        // yt-dlp's generic extractor would fetch any page from the owner's PC.
        ensure!(
            YOUTUBE_HOSTS.contains(&host.as_str()),
            "Only YouTube links can be played."
        );
        ensure!(public_host(&host), "That link points to a private address.");
        return Ok(url.to_string());
    }
    ensure!(
        !lower.contains("://"),
        "Only http and https links can be played."
    );
    let words: String = query
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    Ok(format!("ytsearch1:{}", words.trim()))
}

/// False for localhost names and loopback, private, link-local or unspecified IPs.
fn public_host(host: &str) -> bool {
    if host.is_empty()
        || host == "localhost"
        || [".localhost", ".local", ".internal", ".lan", ".home.arpa"]
            .iter()
            .any(|s| host.ends_with(s))
    {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            !(ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xC0) == 64))
        }
        Ok(IpAddr::V6(ip)) => {
            let first = ip.segments()[0];
            !(ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xFE00) == 0xFC00
                || (first & 0xFFC0) == 0xFE80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| !public_host(&v4.to_string())))
        }
        Err(_) => !host.chars().all(|c| c.is_ascii_digit() || c == '.'),
    }
}

/// A track yt-dlp resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub title: String,
    pub page_url: String,
    pub stream_url: String,
    pub seconds: u64,
    pub filesize: Option<u64>,
    pub headers: Vec<(String, String)>,
}

/// Runs yt-dlp for `target` and checks the result against the duration cap.
async fn resolve(program: &Path, target: &str, max_minutes: u32) -> Result<Resolved> {
    let mut cmd = std::process::Command::new(program);
    cmd.args([
        "--ignore-config",
        "--no-playlist",
        "--playlist-items",
        "1",
        "--no-warnings",
        "--no-progress",
        "--socket-timeout",
        "15",
        "-j",
        "-f",
        FORMAT,
        // Never the generic extractor, which fetches arbitrary pages.
        "--use-extractors",
        "youtube,youtube:tab,youtube:search",
        "--",
        target,
    ]);
    llama::quiet(&mut cmd);
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    let output = tokio::time::timeout(RESOLVE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("Looking up the track timed out. Try again."))?
        .context("Could not run yt-dlp")?;
    if !output.status.success() {
        bail!("{}", ytdlp_error(&String::from_utf8_lossy(&output.stderr)));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| anyhow!("Nothing found for that search."))?;
    parse_track(line, max_minutes)
}

/// A short user-facing reason from yt-dlp's stderr.
pub fn ytdlp_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("ERROR:"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("unknown error")
        .trim();
    let line = line.strip_prefix("ERROR:").unwrap_or(line).trim();
    let lower = line.to_ascii_lowercase();
    if lower.contains("requested format is not available") {
        return "No playable audio format was found for that track (needs Opus or AAC audio)."
            .into();
    }
    if lower.contains("javascript runtime") || lower.contains("js runtime") {
        return "yt-dlp needs a JavaScript runtime (Deno) for YouTube. Install Deno and try again."
            .into();
    }
    if lower.contains("sign in to confirm") {
        return "YouTube is asking this server to sign in (bot check). Try again later.".into();
    }
    let short: String = line.chars().take(200).collect();
    format!("Could not get that track: {short}")
}

/// Parses one yt-dlp `-j` line and applies the duration cap.
pub fn parse_track(json: &str, max_minutes: u32) -> Result<Resolved> {
    let v: Value =
        serde_json::from_str(json).map_err(|_| anyhow!("yt-dlp returned unreadable data."))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let title = s("title").unwrap_or_else(|| "Untitled".into());
    if v.get("is_live").and_then(Value::as_bool) == Some(true) {
        bail!("Live streams can't be played.");
    }
    let seconds = duration_secs(&v)
        .ok_or_else(|| anyhow!("Can't play this: its length is unknown (live stream?)."))?;
    check_duration(seconds, max_minutes)?;
    let protocol = s("protocol").unwrap_or_default();
    ensure!(
        protocol.is_empty() || protocol.starts_with("http"),
        "This track is only available as a stream format I can't play ({protocol})."
    );
    let acodec = s("acodec").unwrap_or_default().to_ascii_lowercase();
    let ext = s("ext").unwrap_or_default().to_ascii_lowercase();
    ensure!(
        playable(&acodec, &ext),
        "No playable audio format was found for that track (needs Opus or AAC audio)."
    );
    let stream_url =
        s("url").ok_or_else(|| anyhow!("No playable audio format was found for that track."))?;
    let headers = v
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| Some((k.clone(), val.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(Resolved {
        title,
        page_url: s("webpage_url")
            .or_else(|| s("original_url"))
            .unwrap_or_default(),
        stream_url,
        seconds,
        filesize: v.get("filesize").and_then(Value::as_u64),
        headers,
    })
}

/// Seconds from "duration" (int or float), else "duration_string" ("1:02:03").
pub fn duration_secs(v: &Value) -> Option<u64> {
    if let Some(d) = v.get("duration").and_then(Value::as_f64)
        && d.is_finite()
        && d > 0.0
    {
        return Some(d.round() as u64);
    }
    let text = v.get("duration_string").and_then(Value::as_str)?;
    let mut total = 0u64;
    let mut parts = 0;
    for part in text.trim().split(':') {
        total = total
            .checked_mul(60)?
            .checked_add(part.trim().parse::<u64>().ok()?)?;
        parts += 1;
    }
    (parts <= 3 && total > 0).then_some(total)
}

/// Enforces max_track_minutes (0 counts as 1).
pub fn check_duration(seconds: u64, max_minutes: u32) -> Result<()> {
    let max = u64::from(max_minutes.max(1)) * 60;
    ensure!(
        seconds <= max,
        "That track is {} long; the limit is {} minute{}.",
        clock(seconds),
        max_minutes.max(1),
        if max_minutes.max(1) == 1 { "" } else { "s" }
    );
    Ok(())
}

/// Audio songbird decodes without ffmpeg: Opus (WebM/Ogg), AAC (M4A/MP4), MP3, Vorbis.
fn playable(acodec: &str, ext: &str) -> bool {
    if acodec == "none" {
        return false;
    }
    let codec_ok = ["opus", "mp4a", "aac", "mp3", "vorbis"]
        .iter()
        .any(|c| acodec.starts_with(c));
    codec_ok
        || (acodec.is_empty() && matches!(ext, "webm" | "m4a" | "mp4" | "mp3" | "ogg" | "opus"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn asset_per_target() {
        assert_eq!(ytdlp_asset("windows", "x86_64").unwrap().0, "yt-dlp.exe");
        assert_eq!(ytdlp_asset("macos", "aarch64").unwrap().0, "yt-dlp_macos");
        assert_eq!(ytdlp_asset("macos", "x86_64").unwrap().0, "yt-dlp_macos");
        assert_eq!(ytdlp_asset("linux", "x86_64").unwrap().0, "yt-dlp_linux");
        assert_eq!(
            ytdlp_asset("linux", "aarch64").unwrap().0,
            "yt-dlp_linux_aarch64"
        );
        assert!(ytdlp_asset("windows", "aarch64").is_none());
        assert!(ytdlp_asset("freebsd", "x86_64").is_none());
        for (_, _, _, sha) in YTDLP_ASSETS {
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert!(ytdlp_dir().ends_with(PathBuf::from("tools/yt-dlp/2026.10.26")));
    }

    #[test]
    fn durations_from_ytdlp_json() {
        assert_eq!(duration_secs(&json!({"duration": 215})), Some(215));
        assert_eq!(duration_secs(&json!({"duration": 214.6})), Some(215));
        assert_eq!(
            duration_secs(&json!({"duration_string": "1:02:03"})),
            Some(3723)
        );
        assert_eq!(
            duration_secs(&json!({"duration_string": "4:05"})),
            Some(245)
        );
        assert_eq!(
            duration_secs(&json!({"duration": null, "duration_string": "x"})),
            None
        );
        assert_eq!(duration_secs(&json!({})), None);
        assert!(check_duration(15 * 60, 15).is_ok());
        assert!(check_duration(15 * 60 + 1, 15).is_err());
        assert!(check_duration(61, 0).is_err());
    }

    #[test]
    fn parses_a_track_and_rejects_bad_ones() {
        let line = json!({
            "title": "Song", "webpage_url": "https://www.youtube.com/watch?v=x",
            "url": "https://rr.example/audio", "duration": 200, "acodec": "opus",
            "ext": "webm", "protocol": "https", "filesize": 1234,
            "http_headers": {"User-Agent": "UA"}
        })
        .to_string();
        let r = parse_track(&line, 15).unwrap();
        assert_eq!(r.title, "Song");
        assert_eq!(r.seconds, 200);
        assert_eq!(r.filesize, Some(1234));
        assert_eq!(r.headers, vec![("User-Agent".into(), "UA".into())]);
        assert!(parse_track(&line, 3).is_err());
        let live = json!({"title": "L", "url": "u", "is_live": true}).to_string();
        assert!(
            parse_track(&live, 15)
                .unwrap_err()
                .to_string()
                .contains("Live")
        );
        let hls =
            json!({"url": "u", "duration": 10, "protocol": "m3u8_native", "acodec": "mp4a.40.2"})
                .to_string();
        assert!(parse_track(&hls, 15).is_err());
        let video_only =
            json!({"url": "u", "duration": 10, "acodec": "none", "ext": "mp4"}).to_string();
        assert!(parse_track(&video_only, 15).is_err());
        let m4a =
            json!({"url": "u", "duration": 10, "acodec": "mp4a.40.2", "ext": "m4a"}).to_string();
        assert!(parse_track(&m4a, 15).is_ok());
        assert!(parse_track("not json", 15).is_err());
    }

    #[test]
    fn queue_limits() {
        assert!(check_queue(&[], 0, 1, 5).is_ok());
        assert!(check_queue(&[1, 1, 1, 1], 0, 1, 5).is_ok());
        assert!(check_queue(&[1, 1, 1, 1, 1], 0, 1, 5).is_err());
        assert!(check_queue(&[1, 1, 1, 1], 1, 1, 5).is_err());
        assert!(check_queue(&[2, 2, 2, 2, 2], 0, 1, 5).is_ok());
        assert!(check_queue(&[1], 0, 1, 0).is_err());
        let full = vec![2u64; MAX_QUEUE_TOTAL];
        assert!(
            check_queue(&full, 0, 1, 5)
                .unwrap_err()
                .to_string()
                .contains("full")
        );
    }

    #[test]
    fn dj_permission_decision() {
        let dj = vec![(42u64, "DJ".to_string())];
        let none: Vec<(u64, String)> = vec![];
        // No DJ role: everyone.
        assert!(dj_allowed(Action::Stop, "", &none, false, false));
        // DJ role by id, mention or name.
        for role in ["42", "<@&42>", "dj", "@DJ"] {
            assert!(dj_allowed(Action::Stop, role, &dj, false, false), "{role}");
            assert!(
                !dj_allowed(Action::Stop, role, &none, false, false),
                "{role}"
            );
        }
        // Own track, admin, pause/resume.
        assert!(dj_allowed(Action::Skip, "42", &none, false, true));
        assert!(!dj_allowed(Action::Skip, "42", &none, false, false));
        assert!(dj_allowed(Action::Leave, "42", &none, true, false));
        assert!(dj_allowed(Action::Pause, "42", &none, false, false));
        assert!(dj_allowed(Action::Resume, "42", &none, false, false));
        assert!(!dj_allowed(Action::Leave, "42", &none, false, true));
    }

    #[test]
    fn targets_and_private_hosts() {
        assert_eq!(
            ytdlp_target(" lofi beats ").unwrap(),
            "ytsearch1:lofi beats"
        );
        assert_eq!(ytdlp_target("-o x").unwrap(), "ytsearch1:-o x");
        assert!(
            ytdlp_target("<https://youtu.be/abc>")
                .unwrap()
                .starts_with("https://youtu.be/")
        );
        assert!(ytdlp_target("").is_err());
        assert!(ytdlp_target("file:///etc/passwd").is_err());
        for bad in [
            "http://localhost:8080/x",
            "http://127.0.0.1/x",
            "http://192.168.1.1/",
            "http://10.0.0.5/",
            "http://[::1]/",
            "http://169.254.169.254/latest",
            "http://router.local/",
        ] {
            assert!(ytdlp_target(bad).is_err(), "{bad}");
        }
        // Only YouTube links: any other public page would reach yt-dlp's generic extractor.
        let err = ytdlp_target("https://example.com/song.mp3").unwrap_err();
        assert_eq!(err.to_string(), "Only YouTube links can be played.");
        assert!(ytdlp_target("https://youtube.com.evil.example/watch?v=x").is_err());
        assert!(
            ytdlp_target("https://music.youtube.com/watch?v=abc").is_ok()
                && ytdlp_target("https://www.youtube.com/watch?v=abc").is_ok()
        );
        assert!(ytdlp_target(&"a".repeat(MAX_QUERY_CHARS + 1)).is_err());
    }

    #[test]
    fn parses_macos_versions() {
        assert_eq!(macos_version("10.14.6\n"), Some((10, 14)));
        assert_eq!(macos_version("10.15"), Some((10, 15)));
        assert_eq!(macos_version("14"), Some((14, 0)));
        assert_eq!(macos_version(""), None);
        assert_eq!(macos_version("x.y"), None);
        assert!(macos_version("10.13.6").unwrap() < (10, 15));
        assert!(macos_version("11.0").unwrap() >= (10, 15));
    }

    #[test]
    fn idle_leave_after_limit() {
        let t0 = Instant::now();
        let limit = Duration::from_secs(300);
        let (since, leave) = idle_step(None, true, true, t0, limit);
        assert_eq!((since, leave), (Some(t0), false));
        let (since, leave) = idle_step(since, true, true, t0 + Duration::from_secs(299), limit);
        assert!(!leave);
        let (_, leave) = idle_step(since, true, true, t0 + Duration::from_secs(300), limit);
        assert!(leave);
        assert_eq!(idle_step(since, true, false, t0, limit), (None, false));
        assert!(idle_step(None, false, false, t0, limit).1);
    }

    #[test]
    fn messages_and_formatting() {
        assert_eq!(clock(187), "3:07");
        assert_eq!(clock(3723), "1:02:03");
        assert_eq!(escape("a*b_@everyone"), "a\\*b\\_@\u{200B}everyone");
        assert!(
            ytdlp_error("WARNING: x\nERROR: [youtube] abc: Requested format is not available")
                .contains("No playable")
        );
        assert_eq!(
            ytdlp_error("ERROR: Video unavailable"),
            "Could not get that track: Video unavailable"
        );
        let info = |t: &str| {
            Arc::new(TrackInfo {
                title: t.into(),
                page_url: String::new(),
                seconds: 60,
                requester: 1,
                requester_name: "kt".into(),
            })
        };
        assert_eq!(format_queue(&[]), "The queue is empty.");
        let q = format_queue(&[info("A"), info("B")]);
        assert!(q.starts_with("Now playing: **A** (1:00)"));
        assert!(q.contains("\n1. B (1:00), kt"));
    }
}

use std::{
    borrow::Cow,
    collections::VecDeque,
    env,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use log::warn;
use reqwest_012::Client as DownloadClient;
use serenity::{
    all::{
        ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateMessage,
        EditMessage, GuildId, MessageId,
    },
    http::Http,
};
use sha2::{Digest, Sha256};
use songbird::{
    Songbird,
    input::{Input, YoutubeDl},
    tracks::{PlayMode, TrackHandle, TrackState},
};
use tokio::{
    fs,
    process::Command,
    sync::{Mutex, Notify, OnceCell},
    time::{sleep, timeout},
};

pub const MUSIC_COMPONENT_PREFIX: &str = "nelfie:music:";

const RUNTIME_TIMEOUT: Duration = Duration::from_secs(180);
const METADATA_TIMEOUT: Duration = Duration::from_secs(90);
pub const MAX_PLAYLIST_TRACKS: usize = 100;
const PANEL_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const HISTORY_LIMIT: usize = 50;
const MAX_RUNTIME_ASSET_BYTES: usize = 250 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct QueueTrackInfo {
    pub id: u64,
    pub title: String,
    pub url: String,
    pub duration: Option<Duration>,
    pub requested_by: String,
}

struct ExtractedYoutubeTrack {
    title: String,
    url: String,
    duration: Option<Duration>,
}

#[derive(Clone, Debug, Default)]
pub struct MusicQueueSnapshot {
    pub current: Option<QueueTrackInfo>,
    pub queued: Vec<QueueTrackInfo>,
    pub volume_percent: u8,
    pub paused: bool,
    pub stopped: bool,
}

#[derive(Clone)]
pub struct MusicSystem {
    players: Arc<DashMap<GuildId, Arc<PlayerSlot>>>,
    songbird: Arc<RwLock<Option<Arc<Songbird>>>>,
    runtime: Arc<OnceCell<Arc<MusicRuntime>>>,
    next_track_id: Arc<AtomicU64>,
}

struct PlayerSlot {
    state: Mutex<GuildPlayer>,
    notify: Notify,
}

struct GuildPlayer {
    queue: VecDeque<QueueTrackInfo>,
    history: VecDeque<QueueTrackInfo>,
    current: Option<CurrentTrack>,
    volume_percent: u8,
    paused: bool,
    stopped: bool,
    panel_enabled: bool,
    panel_closed_by_user: bool,
    panel_channel: Option<ChannelId>,
    http: Option<Arc<Http>>,
    panel_message_channel: Option<ChannelId>,
    panel_message: Option<MessageId>,
    panel_recreate: bool,
    worker_started: bool,
    transition: Option<Transition>,
    last_error: Option<String>,
}

struct CurrentTrack {
    info: QueueTrackInfo,
    handle: TrackHandle,
}

#[derive(Clone, Copy)]
enum Transition {
    Next,
    Previous,
    Stop,
}

struct MusicRuntime {
    ytdlp_program: &'static str,
    deno_path: PathBuf,
    client: DownloadClient,
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct RuntimeVersions {
    yt_dlp: String,
    deno: String,
}

impl Default for GuildPlayer {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            history: VecDeque::new(),
            current: None,
            volume_percent: 100,
            paused: false,
            stopped: false,
            panel_enabled: false,
            panel_closed_by_user: false,
            panel_channel: None,
            http: None,
            panel_message_channel: None,
            panel_message: None,
            panel_recreate: false,
            worker_started: false,
            transition: None,
            last_error: None,
        }
    }
}

impl MusicSystem {
    pub fn new() -> Self {
        Self {
            players: Arc::new(DashMap::new()),
            songbird: Arc::new(RwLock::new(None)),
            runtime: Arc::new(OnceCell::new()),
            next_track_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn set_songbird(&self, manager: Arc<Songbird>) {
        if let Ok(mut current) = self.songbird.write() {
            *current = Some(manager);
        }
    }

    pub fn validate_url(url: &str) -> Result<(), String> {
        validate_youtube_url(url)
    }

    pub async fn enqueue_url(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        http: Arc<Http>,
        url: &str,
        requested_by: &str,
        push_front: bool,
    ) -> Result<Vec<QueueTrackInfo>, String> {
        validate_youtube_url(url)?;
        let runtime = self.ensure_runtime().await?;
        let extracted_tracks = extract_youtube_tracks(&runtime, guild_id, url).await?;
        if extracted_tracks.is_empty() {
            return Err("プレイリストに再生可能な曲がありませんでした。".to_string());
        }
        let tracks = extracted_tracks
            .into_iter()
            .map(|track| QueueTrackInfo {
                id: self.next_track_id.fetch_add(1, Ordering::Relaxed),
                title: track.title,
                url: track.url,
                duration: track.duration,
                requested_by: requested_by.to_string(),
            })
            .collect::<Vec<_>>();

        let slot = self.slot(guild_id);
        {
            let mut player = slot.state.lock().await;
            if player.panel_message.is_none() {
                player.panel_channel = Some(channel_id);
            }
            player.http = Some(http);
            if !player.panel_closed_by_user && player.panel_message.is_none() {
                player.panel_enabled = true;
            }
            if push_front {
                for track in tracks.iter().rev() {
                    player.queue.push_front(track.clone());
                }
            } else {
                player.queue.extend(tracks.iter().cloned());
            }
            player.last_error = None;
        }
        self.wake(guild_id).await;
        Ok(tracks)
    }

    pub async fn set_panel_open(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        http: Arc<Http>,
        open: bool,
    ) {
        let slot = self.slot(guild_id);
        {
            let mut player = slot.state.lock().await;
            player.panel_channel = Some(channel_id);
            player.http = Some(http);
            player.panel_enabled = open;
            player.panel_closed_by_user = !open;
            player.panel_recreate = open;
        }
        self.wake(guild_id).await;
    }

    pub async fn snapshot(&self, guild_id: GuildId) -> MusicQueueSnapshot {
        let Some(slot) = self
            .players
            .get(&guild_id)
            .map(|entry| entry.value().clone())
        else {
            return MusicQueueSnapshot {
                volume_percent: 100,
                ..MusicQueueSnapshot::default()
            };
        };
        let player = slot.state.lock().await;
        MusicQueueSnapshot {
            current: player.current.as_ref().map(|track| track.info.clone()),
            queued: player.queue.iter().cloned().collect(),
            volume_percent: player.volume_percent,
            paused: player.paused,
            stopped: player.stopped,
        }
    }

    pub async fn set_volume(&self, guild_id: GuildId, percent: u8) -> Result<(), String> {
        if percent > 100 {
            return Err("音量は0〜100の範囲で指定してください。".to_string());
        }
        let slot = self.slot(guild_id);
        {
            let mut player = slot.state.lock().await;
            player.volume_percent = percent;
            if let Some(current) = &player.current {
                current
                    .handle
                    .set_volume(f32::from(percent) / 100.0)
                    .map_err(|error| format!("音量を変更できませんでした: {error}"))?;
            }
        }
        self.wake(guild_id).await;
        Ok(())
    }

    pub async fn remove_queued(
        &self,
        guild_id: GuildId,
        track_id: u64,
    ) -> Result<QueueTrackInfo, String> {
        let slot = self.slot(guild_id);
        let removed = {
            let mut player = slot.state.lock().await;
            let index = player
                .queue
                .iter()
                .position(|track| track.id == track_id)
                .ok_or_else(|| "指定した曲はキューにありません。".to_string())?;
            player.queue.remove(index).expect("queue index was checked")
        };
        self.wake(guild_id).await;
        Ok(removed)
    }

    pub async fn clear_queue(&self, guild_id: GuildId) -> usize {
        let slot = self.slot(guild_id);
        let count = {
            let mut player = slot.state.lock().await;
            let count = player.queue.len();
            player.queue.clear();
            count
        };
        self.wake(guild_id).await;
        count
    }

    pub async fn pop_queued(
        &self,
        guild_id: GuildId,
        track_id: u64,
    ) -> Result<QueueTrackInfo, String> {
        let slot = self.slot(guild_id);
        let selected = {
            let mut player = slot.state.lock().await;
            let index = player
                .queue
                .iter()
                .position(|track| track.id == track_id)
                .ok_or_else(|| "指定した曲はキューにありません。".to_string())?;
            let selected = player.queue.remove(index).expect("queue index was checked");
            player.queue.push_front(selected.clone());
            if let Some(handle) = player
                .current
                .as_ref()
                .map(|current| current.handle.clone())
            {
                player.transition = Some(Transition::Next);
                let _ = handle.stop();
            } else {
                player.stopped = false;
            }
            selected
        };
        self.wake(guild_id).await;
        Ok(selected)
    }

    pub async fn shuffle(&self, guild_id: GuildId) -> usize {
        use rand::seq::SliceRandom;

        let slot = self.slot(guild_id);
        let count = {
            let mut player = slot.state.lock().await;
            player.queue.make_contiguous().shuffle(&mut rand::rng());
            player.queue.len()
        };
        self.wake(guild_id).await;
        count
    }

    pub async fn control(&self, guild_id: GuildId, action: &str) -> Result<(), String> {
        let slot = self.slot(guild_id);
        {
            let mut player = slot.state.lock().await;
            match action {
                "previous" => {
                    if let Some((info, handle)) = player
                        .current
                        .as_ref()
                        .map(|current| (current.info.clone(), current.handle.clone()))
                    {
                        player.queue.push_front(info);
                        if let Some(previous) = player.history.pop_back() {
                            player.queue.push_front(previous);
                        }
                        player.transition = Some(Transition::Previous);
                        let _ = handle.stop();
                    } else if let Some(previous) = player.history.pop_back() {
                        player.queue.push_front(previous);
                        player.stopped = false;
                    } else if let Some(next) = player.queue.front() {
                        log::debug!("restart requested for queued music track {}", next.id);
                        player.stopped = false;
                    }
                }
                "next" => {
                    if let Some(handle) = player
                        .current
                        .as_ref()
                        .map(|current| current.handle.clone())
                    {
                        player.transition = Some(Transition::Next);
                        let _ = handle.stop();
                    } else {
                        player.stopped = false;
                    }
                }
                "toggle" => {
                    if let Some(handle) = player
                        .current
                        .as_ref()
                        .map(|current| current.handle.clone())
                    {
                        if player.paused {
                            handle
                                .play()
                                .map_err(|error| format!("再生を再開できませんでした: {error}"))?;
                            player.paused = false;
                        } else {
                            handle
                                .pause()
                                .map_err(|error| format!("一時停止できませんでした: {error}"))?;
                            player.paused = true;
                        }
                    } else {
                        player.stopped = false;
                    }
                }
                "stop" => {
                    if let Some((info, handle)) = player
                        .current
                        .as_ref()
                        .map(|current| (current.info.clone(), current.handle.clone()))
                    {
                        player.queue.push_front(info);
                        player.transition = Some(Transition::Stop);
                        let _ = handle.stop();
                    }
                    player.stopped = true;
                    player.paused = false;
                }
                "volume_down" | "volume_up" => {
                    let adjustment = if action == "volume_up" {
                        10_i16
                    } else {
                        -10_i16
                    };
                    let volume =
                        (i16::from(player.volume_percent) + adjustment).clamp(0, 100) as u8;
                    player.volume_percent = volume;
                    if let Some(handle) = player
                        .current
                        .as_ref()
                        .map(|current| current.handle.clone())
                    {
                        handle
                            .set_volume(f32::from(volume) / 100.0)
                            .map_err(|error| format!("音量を変更できませんでした: {error}"))?;
                    }
                }
                _ => return Err("不明な再生操作です。".to_string()),
            }
        }
        self.wake(guild_id).await;
        Ok(())
    }

    pub async fn handle_component(
        &self,
        guild_id: GuildId,
        message_id: MessageId,
        action: &str,
    ) -> Result<(), String> {
        let Some(slot) = self
            .players
            .get(&guild_id)
            .map(|entry| entry.value().clone())
        else {
            return Ok(());
        };
        {
            let player = slot.state.lock().await;
            if player.panel_message != Some(message_id) {
                return Ok(());
            }
        }
        self.control(guild_id, action).await
    }

    pub async fn clear_guild(&self, guild_id: GuildId) {
        let Some((_, slot)) = self.players.remove(&guild_id) else {
            return;
        };
        let (http, channel_id, message_id) = {
            let mut player = slot.state.lock().await;
            player.stopped = true;
            player.panel_enabled = false;
            if let Some(current) = &player.current {
                let _ = current.handle.stop();
            }
            player.queue.clear();
            player.current = None;
            let message_channel = player
                .panel_message_channel
                .take()
                .or_else(|| player.panel_channel.take());
            (
                player.http.take(),
                message_channel,
                player.panel_message.take(),
            )
        };
        if let (Some(http), Some(channel_id), Some(message_id)) = (http, channel_id, message_id) {
            let _ = channel_id.delete_message(&http, message_id).await;
        }
        slot.notify.notify_one();
    }

    pub async fn clear_all(&self) {
        let guilds = self
            .players
            .iter()
            .map(|entry| *entry.key())
            .collect::<Vec<_>>();
        for guild_id in guilds {
            self.clear_guild(guild_id).await;
        }
    }

    async fn ensure_runtime(&self) -> Result<Arc<MusicRuntime>, String> {
        self.runtime
            .get_or_try_init(|| async { resolve_runtime().await.map(Arc::new) })
            .await
            .cloned()
    }

    fn slot(&self, guild_id: GuildId) -> Arc<PlayerSlot> {
        self.players
            .entry(guild_id)
            .or_insert_with(|| {
                Arc::new(PlayerSlot {
                    state: Mutex::new(GuildPlayer::default()),
                    notify: Notify::new(),
                })
            })
            .value()
            .clone()
    }

    async fn wake(&self, guild_id: GuildId) {
        let slot = self.slot(guild_id);
        let mut player = slot.state.lock().await;
        if !player.worker_started {
            player.worker_started = true;
            let music = self.clone();
            let worker_slot = slot.clone();
            tokio::spawn(async move { music.player_worker(guild_id, worker_slot).await });
        }
        drop(player);
        slot.notify.notify_one();
    }

    async fn player_worker(&self, guild_id: GuildId, slot: Arc<PlayerSlot>) {
        loop {
            let (
                http,
                channel_id,
                old_message_id,
                old_message_channel,
                should_recreate,
                should_edit,
                embed,
                rows,
                idle,
            ) = {
                let mut player = slot.state.lock().await;
                let mut current_state: Option<TrackState> = None;

                if let Some(current) = &player.current {
                    match current.handle.get_info().await {
                        Ok(info) => {
                            if info.playing.is_done() {
                                complete_current(
                                    &mut player,
                                    !matches!(info.playing, PlayMode::Errored(_)),
                                );
                                current_state = None;
                            } else {
                                current_state = Some(info);
                            }
                        }
                        Err(error) => {
                            if error
                                .to_string()
                                .to_ascii_lowercase()
                                .contains("track ended")
                            {
                                complete_current(&mut player, true);
                            } else {
                                warn!(
                                    "failed to retrieve playback state for guild {}: {}",
                                    guild_id.get(),
                                    error
                                );
                                player.last_error =
                                    Some(format!("再生状態を取得できませんでした: {error}"));
                                complete_current(&mut player, false);
                            }
                        }
                    }
                }

                if player.current.is_none() && !player.stopped {
                    if let Some(next) = player.queue.pop_front() {
                        match self.start_track(guild_id, &player, &next).await {
                            Ok(handle) => {
                                player.current = Some(CurrentTrack { info: next, handle });
                                player.paused = false;
                                player.panel_recreate = true;
                                player.last_error = None;
                            }
                            Err(error) => {
                                player.queue.push_front(next);
                                player.stopped = true;
                                player.last_error = Some(error);
                            }
                        }
                    }
                }

                let recreate_requested = player.panel_recreate;
                player.panel_recreate = false;
                let should_recreate =
                    player.panel_enabled && (recreate_requested || player.panel_message.is_none());
                let should_edit =
                    player.panel_enabled && player.panel_message.is_some() && !should_recreate;
                let old_message_id = if should_recreate || !player.panel_enabled {
                    player.panel_message.take()
                } else {
                    None
                };
                let old_message_channel = if old_message_id.is_some() || !player.panel_enabled {
                    player.panel_message_channel.take()
                } else {
                    player.panel_message_channel
                };
                let http = player.http.clone();
                let channel_id = player.panel_channel;
                let embed = build_panel_embed(&player, current_state.as_ref());
                let rows = build_panel_components(guild_id);
                let idle = player.current.is_none() && (player.queue.is_empty() || player.stopped);
                (
                    http,
                    channel_id,
                    old_message_id,
                    old_message_channel,
                    should_recreate,
                    should_edit,
                    embed,
                    rows,
                    idle,
                )
            };

            if let Some(http) = http {
                if let (Some(old_channel_id), Some(message_id)) =
                    (old_message_channel, old_message_id)
                {
                    if let Err(error) = old_channel_id.delete_message(&http, message_id).await {
                        warn!(
                            "failed to delete old music panel for guild {}: {}",
                            guild_id.get(),
                            error
                        );
                    }
                }
                if should_recreate && let Some(channel_id) = channel_id {
                    match channel_id
                        .send_message(
                            &http,
                            CreateMessage::new()
                                .embed(embed.clone())
                                .components(rows.clone()),
                        )
                        .await
                    {
                        Ok(message) => {
                            let mut player = slot.state.lock().await;
                            if player.panel_enabled {
                                player.panel_message = Some(message.id);
                                player.panel_message_channel = Some(channel_id);
                            } else {
                                let _ = channel_id.delete_message(&http, message.id).await;
                            }
                        }
                        Err(error) => warn!(
                            "failed to create music panel for guild {}: {}",
                            guild_id.get(),
                            error
                        ),
                    }
                } else if should_edit {
                    let (message_id, message_channel) = {
                        let player = slot.state.lock().await;
                        (player.panel_message, player.panel_message_channel)
                    };
                    if let (Some(message_id), Some(message_channel)) = (message_id, message_channel)
                    {
                        if let Err(error) = http
                            .edit_message(
                                message_channel,
                                message_id,
                                &EditMessage::new().embed(embed).components(rows),
                                Vec::new(),
                            )
                            .await
                        {
                            warn!(
                                "failed to update music panel for guild {}: {}",
                                guild_id.get(),
                                error
                            );
                        }
                    }
                }
            }

            if idle {
                let mut player = slot.state.lock().await;
                if player.current.is_none() && (player.queue.is_empty() || player.stopped) {
                    player.worker_started = false;
                    return;
                }
            }

            tokio::select! {
                _ = slot.notify.notified() => {},
                _ = sleep(PANEL_UPDATE_INTERVAL) => {},
            }
        }
    }

    async fn start_track(
        &self,
        guild_id: GuildId,
        player: &GuildPlayer,
        track: &QueueTrackInfo,
    ) -> Result<TrackHandle, String> {
        let manager = self
            .songbird
            .read()
            .map_err(|_| "Voice manager lock is unavailable".to_string())?
            .clone()
            .ok_or_else(|| "Voice manager is not initialized".to_string())?;
        let call = manager.get(guild_id).ok_or_else(|| {
            "Bot is not connected to a voice channel. Use /vc join first.".to_string()
        })?;
        let runtime = self.ensure_runtime().await?;
        let input: Input = make_ytdlp(&runtime, track.url.clone()).into();
        let mut handler = call.lock().await;
        let handle = handler.play_input(input);
        if let Err(error) = handle.set_volume(f32::from(player.volume_percent) / 100.0) {
            let _ = handle.stop();
            return Err(format!("音量を設定できませんでした: {error}"));
        }
        Ok(handle)
    }
}

impl Default for MusicSystem {
    fn default() -> Self {
        Self::new()
    }
}

async fn extract_youtube_tracks(
    runtime: &MusicRuntime,
    guild_id: GuildId,
    url: &str,
) -> Result<Vec<ExtractedYoutubeTrack>, String> {
    let js_runtime = format!("deno:{}", runtime.deno_path.display());
    let output = timeout(
        METADATA_TIMEOUT,
        Command::new(runtime.ytdlp_program)
            .kill_on_drop(true)
            .arg("--no-update")
            .arg("--no-cache-dir")
            .arg("--js-runtimes")
            .arg(js_runtime)
            .arg("--yes-playlist")
            .arg("--ignore-errors")
            .arg("--flat-playlist")
            .arg("--playlist-end")
            .arg(MAX_PLAYLIST_TRACKS.to_string())
            .arg("--dump-json")
            .arg(url)
            .output(),
    )
    .await;

    let output = match output {
        Err(_) => {
            warn!(
                "timed out retrieving YouTube playlist metadata for guild {}",
                guild_id.get()
            );
            return Err("YouTubeの曲情報取得がタイムアウトしました。".to_string());
        }
        Ok(Err(error)) => {
            warn!(
                "failed to start YouTube playlist metadata extraction for guild {}: {}",
                guild_id.get(),
                error
            );
            return Err(
                "YouTubeから曲情報を取得できませんでした。詳細はログを確認してください。"
                    .to_string(),
            );
        }
        Ok(Ok(output)) => output,
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        warn!(
            "YouTube playlist metadata extraction failed for guild {}: {}",
            guild_id.get(),
            stderr.trim()
        );
        return Err(summarize_youtube_metadata_error(&stderr));
    }
    if !stderr.trim().is_empty() {
        warn!(
            "YouTube playlist metadata extraction warning for guild {}: {}",
            guild_id.get(),
            stderr.trim()
        );
    }

    match parse_flat_youtube_tracks(&output.stdout) {
        Ok(tracks) => Ok(tracks),
        Err(error) => {
            warn!(
                "failed to parse YouTube playlist metadata for guild {}: {}",
                guild_id.get(),
                error
            );
            Err(
                "YouTubeから曲情報を取得できませんでした。詳細はログを確認してください。"
                    .to_string(),
            )
        }
    }
}

fn parse_flat_youtube_tracks(
    output: &[u8],
) -> Result<Vec<ExtractedYoutubeTrack>, serde_json::Error> {
    let output = String::from_utf8_lossy(output);
    let mut tracks = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let entry: serde_json::Value = serde_json::from_str(line)?;
        if let Some(entries) = entry.get("entries").and_then(serde_json::Value::as_array) {
            for entry in entries {
                append_flat_youtube_track(entry, &mut tracks);
            }
        } else {
            append_flat_youtube_track(&entry, &mut tracks);
        }
        if tracks.len() >= MAX_PLAYLIST_TRACKS {
            break;
        }
    }
    tracks.truncate(MAX_PLAYLIST_TRACKS);
    Ok(tracks)
}

fn append_flat_youtube_track(entry: &serde_json::Value, tracks: &mut Vec<ExtractedYoutubeTrack>) {
    if tracks.len() >= MAX_PLAYLIST_TRACKS {
        return;
    }

    let video_url = ["webpage_url", "url"]
        .into_iter()
        .filter_map(|field| entry.get(field).and_then(serde_json::Value::as_str))
        .map(str::trim)
        .find(|candidate| validate_youtube_url(candidate).is_ok())
        .map(str::to_string)
        .or_else(|| {
            let id = entry.get("id")?.as_str()?.trim();
            (!id.is_empty()
                && id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character)))
            .then(|| format!("https://www.youtube.com/watch?v={id}"))
        });
    let Some(url) = video_url else {
        return;
    };

    let title = entry
        .get("title")
        .and_then(serde_json::Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .or_else(|| entry.get("id").and_then(serde_json::Value::as_str))
        .unwrap_or(&url)
        .to_string();
    let duration = entry
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    tracks.push(ExtractedYoutubeTrack {
        title,
        url,
        duration,
    });
}

fn make_ytdlp(runtime: &MusicRuntime, url: String) -> YoutubeDl<'static> {
    let args = vec![
        "--no-update".to_string(),
        "--no-cache-dir".to_string(),
        "--js-runtimes".to_string(),
        format!("deno:{}", runtime.deno_path.display()),
    ];
    YoutubeDl::new_ytdl_like(
        runtime.ytdlp_program,
        runtime.client.clone(),
        Cow::Owned(url),
    )
    .user_args(args)
}

fn summarize_youtube_metadata_error(error: &str) -> String {
    let error = error.to_ascii_lowercase();
    if error.contains("only available to music premium members")
        || error.contains("only available to youtube music premium members")
    {
        "この動画はYouTube Music Premium会員限定のため再生できません。".to_string()
    } else if error.contains("private video") || error.contains("this video is private") {
        "この動画は非公開のため再生できません。".to_string()
    } else if error.contains("region") && error.contains("unavailable") {
        "この動画は地域制限のため再生できません。".to_string()
    } else if error.contains("unavailable")
        || error.contains("has been removed")
        || error.contains("video is not available")
    {
        "この動画は現在利用できません。".to_string()
    } else {
        "YouTubeから曲情報を取得できませんでした。詳細はログを確認してください。".to_string()
    }
}

fn validate_youtube_url(url: &str) -> Result<(), String> {
    let parsed =
        reqwest_012::Url::parse(url).map_err(|_| "有効なURLを指定してください。".to_string())?;
    if parsed.scheme() != "https" {
        return Err("HTTPSのYouTube URLを指定してください。".to_string());
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let accepted = matches!(
        host.as_str(),
        "youtube.com"
            | "www.youtube.com"
            | "m.youtube.com"
            | "music.youtube.com"
            | "youtu.be"
            | "youtube-nocookie.com"
            | "www.youtube-nocookie.com"
    );
    if !accepted {
        return Err("YouTubeまたはYouTube MusicのURLを指定してください。".to_string());
    }
    Ok(())
}

fn build_panel_embed(player: &GuildPlayer, state: Option<&TrackState>) -> CreateEmbed {
    let panel_title = player
        .current
        .as_ref()
        .map(|current| truncate_chars(&current.info.title, 250))
        .unwrap_or_else(|| {
            if player.stopped {
                "停止中".to_string()
            } else {
                "🎵 再生待ち".to_string()
            }
        });
    let mut embed = CreateEmbed::new().title(panel_title).color(0x5865F2);
    if let Some(current) = &player.current {
        embed = embed.url(current.info.url.clone());
        if !current.info.requested_by.is_empty() {
            embed = embed.description(format!("リクエスト: {}", current.info.requested_by));
        }
        let position = state.map(|state| state.position).unwrap_or_default();
        let duration = current.info.duration;
        let progress = progress_bar(position, duration);
        embed = embed.field(
            "再生位置",
            format!(
                "{} {} / {}",
                progress,
                format_duration(position),
                duration
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".to_string())
            ),
            false,
        );
        embed = embed.field(
            "状態",
            if player.paused {
                "一時停止中"
            } else {
                "再生中"
            },
            true,
        );
    } else if player.stopped {
        embed = embed.description("停止中です。再生ボタンでキューの先頭から再開できます。");
    } else if player.queue.is_empty() {
        embed = embed.description("再生待ちの曲はありません。");
    } else {
        embed = embed.description("キューから次の曲を準備しています…");
    }
    embed = embed.field("音量", format!("{}%", player.volume_percent), true);
    embed = embed.field("待機キュー", format!("{}曲", player.queue.len()), true);
    if let Some(error) = &player.last_error {
        embed = embed.field("エラー", truncate_chars(error, 900), false);
    }
    embed
}

fn complete_current(player: &mut GuildPlayer, completed: bool) {
    let reason = player.transition.take();
    if let Some(ended) = player.current.take() {
        if !matches!(reason, Some(Transition::Previous | Transition::Stop))
            && (completed || matches!(reason, Some(Transition::Next)))
        {
            player.history.push_back(ended.info);
            while player.history.len() > HISTORY_LIMIT {
                player.history.pop_front();
            }
        }
    }
    player.paused = false;
    if matches!(reason, Some(Transition::Stop)) {
        player.stopped = true;
    }
    player.panel_recreate = true;
}

fn build_panel_components(guild_id: GuildId) -> Vec<CreateActionRow> {
    let button = |action: &str, label: &str, style: ButtonStyle| {
        CreateButton::new(format!(
            "{MUSIC_COMPONENT_PREFIX}{}:{action}",
            guild_id.get()
        ))
        .label(label)
        .style(style)
    };
    vec![
        CreateActionRow::Buttons(vec![
            button("previous", "前へ", ButtonStyle::Secondary),
            button("toggle", "再生 / 一時停止", ButtonStyle::Primary),
            button("stop", "停止", ButtonStyle::Danger),
            button("next", "次へ", ButtonStyle::Secondary),
        ]),
        CreateActionRow::Buttons(vec![
            button("volume_down", "音量 −", ButtonStyle::Secondary),
            button("volume_up", "音量 ＋", ButtonStyle::Secondary),
        ]),
    ]
}

fn progress_bar(position: Duration, duration: Option<Duration>) -> String {
    let Some(duration) = duration.filter(|duration| !duration.is_zero()) else {
        return "━━━━━━━━━━━━━━━━━━━━".to_string();
    };
    let fraction = (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0);
    let filled = (fraction * 20.0).round() as usize;
    format!("{}{}", "█".repeat(filled), "▁".repeat(20 - filled))
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds / 60) % 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

async fn resolve_runtime() -> Result<MusicRuntime, String> {
    let directory = runtime_directory()?;
    fs::create_dir_all(&directory)
        .await
        .map_err(|error| format!("音楽ランタイムのキャッシュ先を作成できません: {error}"))?;
    let (yt_asset_name, deno_asset_name) = runtime_asset_names()?;
    let yt_path = directory.join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    let deno_path = directory.join(if cfg!(windows) { "deno.exe" } else { "deno" });
    let versions_path = directory.join("versions.json");
    let client = DownloadClient::builder()
        .user_agent(concat!("Nelfie/", env!("CARGO_PKG_VERSION")))
        .timeout(RUNTIME_TIMEOUT)
        .build()
        .map_err(|error| format!("音楽ランタイム用HTTPクライアントを作成できません: {error}"))?;

    let latest = fetch_latest_releases(&client).await;
    match latest {
        Ok((yt_release, deno_release)) => {
            let versions = RuntimeVersions {
                yt_dlp: yt_release.tag_name.clone(),
                deno: deno_release.tag_name.clone(),
            };
            let cached_versions = fs::read(&versions_path)
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<RuntimeVersions>(&bytes).ok());
            let valid_cache = cached_versions
                .as_ref()
                .map(|cached| cached.yt_dlp == versions.yt_dlp && cached.deno == versions.deno)
                .unwrap_or(false)
                && fs::try_exists(&yt_path).await.unwrap_or(false)
                && fs::try_exists(&deno_path).await.unwrap_or(false);
            if !valid_cache {
                let yt_asset = find_asset(&yt_release, &yt_asset_name, "yt-dlp")?;
                let deno_asset = find_asset(&deno_release, &deno_asset_name, "Deno")?;
                install_yt_dlp(&client, yt_asset, &yt_path).await?;
                install_deno(&client, deno_asset, &deno_path).await?;
                let encoded = serde_json::to_vec(&versions).map_err(|error| {
                    format!("ランタイムバージョン情報を保存できません: {error}")
                })?;
                fs::write(&versions_path, encoded).await.map_err(|error| {
                    format!("ランタイムバージョン情報を保存できません: {error}")
                })?;
            }
        }
        Err(error) => {
            let cached = fs::try_exists(&yt_path).await.unwrap_or(false)
                && fs::try_exists(&deno_path).await.unwrap_or(false);
            if !cached {
                return Err(format!(
                    "YouTube再生に必要なランタイムを取得できません: {error}"
                ));
            }
            warn!("GitHub release lookup failed; using cached music runtime: {error}");
        }
    }

    let ytdlp_program = Box::leak(yt_path.to_string_lossy().into_owned().into_boxed_str());
    Ok(MusicRuntime {
        ytdlp_program,
        deno_path,
        client,
    })
}

fn runtime_directory() -> Result<PathBuf, String> {
    let executable_path = env::current_exe()
        .map_err(|error| format!("実行ファイルの場所を取得できません: {error}"))?;
    let executable_directory = executable_path
        .parent()
        .ok_or_else(|| "実行ファイルの配置先を特定できません。".to_string())?;
    Ok(executable_directory.join("runtime").join("music"))
}

fn runtime_asset_names() -> Result<(String, String), String> {
    let (yt_dlp, deno) = match (env::consts::OS, env::consts::ARCH) {
        ("windows", "x86_64") => (
            "yt-dlp.exe".to_string(),
            "deno-x86_64-pc-windows-msvc.zip".to_string(),
        ),
        ("windows", "aarch64") => (
            "yt-dlp_arm64.exe".to_string(),
            "deno-aarch64-pc-windows-msvc.zip".to_string(),
        ),
        ("linux", "x86_64") => (
            "yt-dlp_linux".to_string(),
            "deno-x86_64-unknown-linux-gnu.zip".to_string(),
        ),
        ("linux", "aarch64") => (
            "yt-dlp_linux_aarch64".to_string(),
            "deno-aarch64-unknown-linux-gnu.zip".to_string(),
        ),
        (os, arch) => return Err(format!("音楽ランタイムは未対応の環境です: {os}/{arch}")),
    };
    Ok((yt_dlp, deno))
}

async fn fetch_latest_releases(client: &DownloadClient) -> Result<(Release, Release), String> {
    let yt = client
        .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
        .send()
        .await
        .and_then(reqwest_012::Response::error_for_status)
        .map_err(|error| format!("yt-dlp release API: {error}"))?
        .json::<Release>()
        .await
        .map_err(|error| format!("yt-dlp release response: {error}"))?;
    let deno = client
        .get("https://api.github.com/repos/denoland/deno/releases/latest")
        .send()
        .await
        .and_then(reqwest_012::Response::error_for_status)
        .map_err(|error| format!("Deno release API: {error}"))?
        .json::<Release>()
        .await
        .map_err(|error| format!("Deno release response: {error}"))?;
    Ok((yt, deno))
}

fn find_asset<'a>(
    release: &'a Release,
    name: &str,
    owner: &str,
) -> Result<&'a ReleaseAsset, String> {
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| {
            format!(
                "{owner} {} release has no asset named {name}",
                release.tag_name
            )
        })?;
    let expected_prefix = match owner {
        "yt-dlp" => "https://github.com/yt-dlp/yt-dlp/releases/download/",
        "Deno" => "https://github.com/denoland/deno/releases/download/",
        _ => return Err("不明なランタイム配布元です。".to_string()),
    };
    if !asset.browser_download_url.starts_with(expected_prefix) {
        return Err(format!(
            "{owner} asset URL is not an official GitHub release URL"
        ));
    }
    if asset.size == 0 || asset.size as usize > MAX_RUNTIME_ASSET_BYTES {
        return Err(format!("{owner} runtime asset has an invalid size"));
    }
    if asset
        .digest
        .as_deref()
        .is_none_or(|digest| !digest.starts_with("sha256:"))
    {
        return Err(format!(
            "{owner} runtime release does not publish a SHA-256 asset digest"
        ));
    }
    Ok(asset)
}

async fn download_asset(client: &DownloadClient, asset: &ReleaseAsset) -> Result<Vec<u8>, String> {
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .and_then(reqwest_012::Response::error_for_status)
        .map_err(|error| format!("{} download failed: {error}", asset.name))?
        .bytes()
        .await
        .map_err(|error| format!("{} download failed: {error}", asset.name))?;
    if bytes.len() != asset.size as usize || bytes.len() > MAX_RUNTIME_ASSET_BYTES {
        return Err(format!(
            "{} download size did not match release metadata",
            asset.name
        ));
    }
    let actual = format!("sha256:{:x}", Sha256::digest(&bytes));
    if asset.digest.as_deref() != Some(actual.as_str()) {
        return Err(format!("{} SHA-256 verification failed", asset.name));
    }
    Ok(bytes.to_vec())
}

async fn install_yt_dlp(
    client: &DownloadClient,
    asset: &ReleaseAsset,
    destination: &Path,
) -> Result<(), String> {
    let bytes = download_asset(client, asset).await?;
    write_runtime_file(destination, &bytes).await
}

async fn install_deno(
    client: &DownloadClient,
    asset: &ReleaseAsset,
    destination: &Path,
) -> Result<(), String> {
    let archive = download_asset(client, asset).await?;
    let deno = tokio::task::spawn_blocking(move || extract_deno(&archive))
        .await
        .map_err(|error| format!("Deno archive extraction task failed: {error}"))??;
    write_runtime_file(destination, &deno).await
}

fn extract_deno(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("Deno archive is invalid: {error}"))?;
    let expected = if cfg!(windows) { "deno.exe" } else { "deno" };
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("Deno archive entry is invalid: {error}"))?;
        if Path::new(entry.name())
            .file_name()
            .and_then(|name| name.to_str())
            != Some(expected)
        {
            continue;
        }
        if entry.size() == 0 || entry.size() as usize > MAX_RUNTIME_ASSET_BYTES {
            return Err("Deno executable in archive has an invalid size".to_string());
        }
        let mut output = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut output)
            .map_err(|error| format!("Deno executable extraction failed: {error}"))?;
        if output.len() != entry.size() as usize {
            return Err("Deno executable extraction was incomplete".to_string());
        }
        return Ok(output);
    }
    Err(format!("Deno archive does not contain {expected}"))
}

async fn write_runtime_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!(
        "{}.part",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("bin")
    ));
    fs::write(&temporary, bytes)
        .await
        .map_err(|error| format!("runtime file write failed: {error}"))?;
    if path.exists() {
        fs::remove_file(path)
            .await
            .map_err(|error| format!("old runtime file replacement failed: {error}"))?;
    }
    fs::rename(&temporary, path)
        .await
        .map_err(|error| format!("runtime file install failed: {error}"))?;
    set_executable(path).await?;
    Ok(())
}

#[cfg(unix)]
async fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .await
        .map_err(|error| format!("runtime executable permission setup failed: {error}"))
}

#[cfg(not(unix))]
async fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

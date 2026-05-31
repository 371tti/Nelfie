use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};

use dashmap::DashMap;
use log::{debug, info, warn};
use serenity::all::{ChannelId, GuildId};
use songbird::{
    Songbird, driver::Bitrate as SongbirdBitrate, input::Input as SongbirdInput,
    tracks::TrackHandle,
};
use tokio::{
    sync::{OnceCell, Semaphore, mpsc},
    task::JoinHandle,
    time::{Duration, sleep, timeout},
};

use super::{
    core_runtime::CoreRuntime,
    text::{normalize_tts_text, normalize_tts_text_full, split_tts_segments},
    types::{GuildVoiceConfig, SpeakOptions, SpeakRequest, VoiceCoreConfig},
};

const LONG_TTS_TEXT_MIN_SPEED: f32 = 1.4;
const LONG_TTS_TEXT_THRESHOLD_CHARS: usize = 64;
const SEQUENTIAL_QUEUE_CAPACITY: usize = 16;
const DEFAULT_PARALLEL_READ_COUNT: usize = 1;
const MAX_PARALLEL_READ_COUNT: usize = 4;
const TRACK_END_POLL_INTERVAL: Duration = Duration::from_millis(15);
const TRACK_END_MAX_CONSECUTIVE_ERRORS: usize = 5;
const QUEUE_FULL_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const SEGMENT_SYNTHESIS_TIMEOUT: Duration = Duration::from_secs(45);
const SEGMENT_PLAYBACK_TIMEOUT: Duration = Duration::from_secs(30);

fn normalize_parallel_count(parallel_count: usize) -> usize {
    parallel_count.clamp(DEFAULT_PARALLEL_READ_COUNT, MAX_PARALLEL_READ_COUNT)
}

#[derive(Clone)]
pub struct VoiceSystem {
    default_speaker: u32,
    core_config: VoiceCoreConfig,
    core_runtime: Arc<OnceCell<Arc<CoreRuntime>>>,
    core_warmup_started: Arc<AtomicBool>,
    songbird: Arc<RwLock<Option<Arc<Songbird>>>>,
    guild_configs: Arc<DashMap<GuildId, GuildVoiceConfig>>,
    channel_queues: Arc<DashMap<ChannelId, mpsc::Sender<SpeakRequest>>>,
    channel_parallel_counts: Arc<DashMap<ChannelId, usize>>,
    channel_parallel_semaphores: Arc<DashMap<ChannelId, Arc<Semaphore>>>,
    last_errors: Arc<DashMap<GuildId, String>>,
}

impl VoiceSystem {
    pub fn new(default_speaker: u32, core_config: VoiceCoreConfig) -> Self {
        Self {
            default_speaker,
            core_config,
            core_runtime: Arc::new(OnceCell::new()),
            core_warmup_started: Arc::new(AtomicBool::new(false)),
            songbird: Arc::new(RwLock::new(None)),
            guild_configs: Arc::new(DashMap::new()),
            channel_queues: Arc::new(DashMap::new()),
            channel_parallel_counts: Arc::new(DashMap::new()),
            channel_parallel_semaphores: Arc::new(DashMap::new()),
            last_errors: Arc::new(DashMap::new()),
        }
    }

    pub fn warmup_async(&self) {
        if self.core_warmup_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let this = self.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            match this.ensure_core_initialized().await {
                Ok(_) => {
                    info!(
                        "VOICEVOX core preload completed in {} ms",
                        started.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    warn!("VOICEVOX core preload failed: {}", e);
                }
            }
        });
    }

    pub async fn initialize_on_startup(&self) -> Result<(), String> {
        self.core_warmup_started.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        self.ensure_core_initialized().await?;
        info!(
            "VOICEVOX startup initialization completed in {} ms",
            started.elapsed().as_millis()
        );
        Ok(())
    }

    pub fn set_songbird(&self, manager: Arc<Songbird>) {
        if let Ok(mut w) = self.songbird.write() {
            *w = Some(manager);
        }
    }

    pub fn clear_all(&self) {
        self.channel_queues.clear();
        self.channel_parallel_counts.clear();
        self.channel_parallel_semaphores.clear();
        self.guild_configs.clear();
        self.last_errors.clear();
    }

    pub fn core_summary(&self) -> String {
        let acceleration = if self
            .core_config
            .acceleration_mode
            .trim()
            .eq_ignore_ascii_case("auto")
        {
            "cpu(auto-mapped)".to_string()
        } else {
            self.core_config.acceleration_mode.clone()
        };

        format!(
            "standalone(acceleration={}, cpu_threads={}, load_all_models=forced(true), output_sampling_rate={}, output_stereo=fixed(false))",
            acceleration, self.core_config.cpu_threads, self.core_config.output_sampling_rate,
        )
    }

    pub fn last_error(&self, guild_id: GuildId) -> Option<String> {
        self.last_errors.get(&guild_id).map(|v| v.value().clone())
    }

    fn set_last_error(&self, guild_id: GuildId, err: impl Into<String>) {
        self.last_errors.insert(guild_id, err.into());
    }

    fn clear_last_error(&self, guild_id: GuildId) {
        self.last_errors.remove(&guild_id);
    }

    fn default_guild_config(&self) -> GuildVoiceConfig {
        GuildVoiceConfig {
            text_channel_id: None,
            auto_read: false,
            speaker: self.default_speaker,
        }
    }

    pub fn config(&self, guild_id: GuildId) -> GuildVoiceConfig {
        self.guild_configs
            .get(&guild_id)
            .map(|v| v.clone())
            .unwrap_or_else(|| self.default_guild_config())
    }

    pub fn set_speaker(&self, guild_id: GuildId, speaker: u32) {
        self.guild_configs
            .entry(guild_id)
            .and_modify(|cfg| cfg.speaker = speaker)
            .or_insert_with(|| GuildVoiceConfig {
                speaker,
                ..self.default_guild_config()
            });
    }

    pub fn set_auto_read(&self, guild_id: GuildId, enabled: bool, text_channel: Option<ChannelId>) {
        self.guild_configs
            .entry(guild_id)
            .and_modify(|cfg| {
                cfg.auto_read = enabled;
                if text_channel.is_some() {
                    cfg.text_channel_id = text_channel;
                }
            })
            .or_insert_with(|| GuildVoiceConfig {
                text_channel_id: text_channel,
                auto_read: enabled,
                ..self.default_guild_config()
            });
    }

    pub fn set_channel_parallel_count(
        &self,
        channel_id: ChannelId,
        parallel_count: usize,
    ) -> usize {
        let normalized = normalize_parallel_count(parallel_count);

        if self
            .channel_parallel_counts
            .get(&channel_id)
            .map(|value| *value.value() == normalized)
            .unwrap_or(false)
        {
            return normalized;
        }

        self.channel_parallel_counts.insert(channel_id, normalized);
        self.channel_parallel_semaphores
            .insert(channel_id, Arc::new(Semaphore::new(normalized)));
        normalized
    }

    pub fn channel_parallel_count(&self, channel_id: ChannelId) -> usize {
        self.channel_parallel_counts
            .get(&channel_id)
            .map(|v| *v.value())
            .unwrap_or(DEFAULT_PARALLEL_READ_COUNT)
    }

    pub fn max_parallel_read_count(&self) -> usize {
        MAX_PARALLEL_READ_COUNT
    }

    pub fn sequential_queue_capacity(&self) -> usize {
        SEQUENTIAL_QUEUE_CAPACITY
    }

    pub async fn join_voice(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<(), String> {
        let manager = self
            .songbird_manager()
            .ok_or_else(|| "Voice manager is not initialized".to_string())?;

        manager
            .join(guild_id, channel_id)
            .await
            .map_err(|e| format!("Failed to join voice channel: {e:?}"))?;

        if let Some(call) = manager.get(guild_id) {
            let mut handler = call.lock().await;
            // DiscordはOpus配信のため、可能な限り高ビットレートにして劣化を抑える。
            handler.set_bitrate(SongbirdBitrate::Max);
        }

        self.clear_last_error(guild_id);
        Ok(())
    }

    pub async fn leave_voice(&self, guild_id: GuildId) -> Result<(), String> {
        let manager = self
            .songbird_manager()
            .ok_or_else(|| "Voice manager is not initialized".to_string())?;

        manager
            .remove(guild_id)
            .await
            .map_err(|e| format!("Failed to leave voice channel: {e:?}"))?;

        self.clear_last_error(guild_id);

        Ok(())
    }

    pub async fn current_voice_channel_raw(&self, guild_id: GuildId) -> Option<u64> {
        let manager = self.songbird_manager()?;
        let call = manager.get(guild_id)?;
        let handler = call.lock().await;
        handler.current_channel().map(|cid| cid.0.get())
    }

    pub async fn speak(
        &self,
        guild_id: GuildId,
        text: impl Into<String>,
        options: SpeakOptions,
    ) -> Result<(), String> {
        let SpeakOptions {
            speaker,
            speed_scale,
            pitch_scale,
            pan,
            channel_id,
            parallel_count,
        } = options;

        let text = normalize_tts_text(&text.into());
        if text.is_empty() {
            return Ok(());
        }

        let speed_scale = enforce_min_speed_for_long_text(&text, speed_scale);

        if self.songbird_manager().is_none() {
            let err = "Voice manager is not initialized".to_string();
            self.set_last_error(guild_id, err.clone());
            return Err(err);
        }

        if self.current_voice_channel_raw(guild_id).await.is_none() {
            let err = "Not connected to a voice channel. Run /vc_join first.".to_string();
            self.set_last_error(guild_id, err.clone());
            return Err(err);
        }

        let cfg = self.config(guild_id);
        let resolved_speaker = speaker.unwrap_or(cfg.speaker);
        let normalized_parallel_count = self.set_channel_parallel_count(channel_id, parallel_count);
        let req = SpeakRequest {
            guild_id,
            channel_id,
            text,
            speaker: resolved_speaker,
            speed_scale,
            pitch_scale,
            pan: pan.map(|v| v.clamp(-1.0, 1.0)),
            parallel_count: normalized_parallel_count,
        };

        let queue = self.ensure_queue(channel_id).await;

        debug!(
            "enqueue voice request guild={} channel={} chars={} speaker={} parallel={}",
            guild_id.get(),
            channel_id.get(),
            req.text.chars().count(),
            req.speaker,
            normalized_parallel_count,
        );

        match queue.try_send(req) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(req)) => {
                match timeout(QUEUE_FULL_WAIT_TIMEOUT, queue.send(req)).await {
                    Ok(Ok(())) => {
                        debug!(
                            "voice queue recovered after waiting (guild={}, channel={})",
                            guild_id.get(),
                            channel_id.get()
                        );
                        Ok(())
                    }
                    Ok(Err(_)) => {
                        let err = "Voice queue is unavailable".to_string();
                        self.set_last_error(guild_id, err.clone());
                        Err(err)
                    }
                    Err(_) => {
                        let err = format!(
                            "Voice queue remained full for {:?} (sequential limit={SEQUENTIAL_QUEUE_CAPACITY})",
                            QUEUE_FULL_WAIT_TIMEOUT
                        );
                        self.set_last_error(guild_id, err.clone());
                        Err(err)
                    }
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let err = "Voice queue is unavailable".to_string();
                self.set_last_error(guild_id, err.clone());
                Err(err)
            }
        }
    }

    pub async fn synthesize_wav(
        &self,
        text: impl Into<String>,
        speaker: u32,
        speed_scale: Option<f32>,
        pitch_scale: Option<f32>,
        pan: Option<f32>,
    ) -> Result<Vec<u8>, String> {
        let text = normalize_tts_text_full(&text.into());
        if text.is_empty() {
            return Err("Text is empty".to_string());
        }

        let speed_scale = enforce_min_speed_for_long_text(&text, speed_scale);
        let segments = split_tts_segments(&text);
        if segments.is_empty() {
            return Err("Text is empty".to_string());
        }

        let mut wav_segments = Vec::with_capacity(segments.len());
        for segment in segments {
            wav_segments.push(
                self.synthesize(&segment, speaker, speed_scale, pitch_scale)
                    .await?,
            );
        }

        let wav = concatenate_wav_segments(wav_segments)?;

        apply_pan_to_wav(wav, pan.map(|v| v.clamp(-1.0, 1.0)))
    }

    async fn ensure_queue(&self, channel_id: ChannelId) -> mpsc::Sender<SpeakRequest> {
        if let Some(existing) = self.channel_queues.get(&channel_id) {
            return existing.clone();
        }

        let (tx, mut rx) = mpsc::channel::<SpeakRequest>(SEQUENTIAL_QUEUE_CAPACITY);
        match self.channel_queues.entry(channel_id) {
            dashmap::mapref::entry::Entry::Occupied(existing) => {
                return existing.get().clone();
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(tx.clone());
            }
        }

        self.channel_parallel_counts
            .entry(channel_id)
            .or_insert(DEFAULT_PARALLEL_READ_COUNT);
        self.channel_parallel_semaphores
            .entry(channel_id)
            .or_insert_with(|| Arc::new(Semaphore::new(DEFAULT_PARALLEL_READ_COUNT)));

        let this = self.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let semaphore = this
                    .channel_parallel_semaphores
                    .get(&channel_id)
                    .map(|entry| Arc::clone(entry.value()))
                    .unwrap_or_else(|| Arc::new(Semaphore::new(DEFAULT_PARALLEL_READ_COUNT)));

                let Ok(permit) = semaphore.acquire_owned().await else {
                    break;
                };

                let this = this.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    this.process_speak_request(req).await;
                });
            }

            this.channel_queues.remove(&channel_id);
            this.channel_parallel_semaphores.remove(&channel_id);
            this.channel_parallel_counts.remove(&channel_id);
            debug!("voice queue worker ended for text channel {}", channel_id);
        });

        tx
    }

    async fn process_speak_request(&self, req: SpeakRequest) {
        let segments = split_tts_segments(&req.text);
        if segments.is_empty() {
            return;
        }

        debug!(
            "start speak request guild={} channel={} segments={}",
            req.guild_id.get(),
            req.channel_id.get(),
            segments.len(),
        );

        // 1セグメント先読みで、現在セグメント再生中に次セグメントの合成を進める
        // これにより、同一メッセージ内の順序を保ちつつ区切り間の待ちを減らす
        // あと1セグメント先読み以上だと集中時にメモリ爆死する可能性があるから、、メモリ圧は上げないよーに
        let allow_overlap_playback = req.parallel_count > DEFAULT_PARALLEL_READ_COUNT;
        let mut segments = segments.into_iter();
        let Some(first_segment) = segments.next() else {
            return;
        };

        let mut in_flight = self.spawn_segment_synthesis(
            first_segment,
            req.speaker,
            req.speed_scale,
            req.pitch_scale,
            req.pan,
        );

        loop {
            let wav = match timeout(SEGMENT_SYNTHESIS_TIMEOUT, &mut in_flight).await {
                Ok(joined) => match joined {
                    Ok(Ok(wav)) => wav,
                    Ok(Err(e)) => {
                        warn!(
                            "failed to synthesize voice (guild={}, channel={}): {}",
                            req.guild_id.get(),
                            req.channel_id.get(),
                            e
                        );
                        self.set_last_error(req.guild_id, e);
                        return;
                    }
                    Err(e) => {
                        let err = format!("failed to join segment synthesis task: {e}");
                        warn!(
                            "{} (guild={}, channel={})",
                            err,
                            req.guild_id.get(),
                            req.channel_id.get(),
                        );
                        self.set_last_error(req.guild_id, err);
                        return;
                    }
                },
                Err(_) => {
                    in_flight.abort();
                    let err = format!(
                        "segment synthesis timed out after {:?}",
                        SEGMENT_SYNTHESIS_TIMEOUT
                    );
                    warn!(
                        "{} (guild={}, channel={})",
                        err,
                        req.guild_id.get(),
                        req.channel_id.get(),
                    );
                    self.set_last_error(req.guild_id, err);
                    return;
                }
            };

            let next_in_flight = segments.next().map(|segment| {
                self.spawn_segment_synthesis(
                    segment,
                    req.speaker,
                    req.speed_scale,
                    req.pitch_scale,
                    req.pan,
                )
            });

            match timeout(
                SEGMENT_PLAYBACK_TIMEOUT,
                self.play_wav_segment(req.guild_id, wav, allow_overlap_playback),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if let Some(handle) = next_in_flight {
                        handle.abort();
                    }
                    warn!(
                        "failed to play voice segment (guild={}, channel={}): {}",
                        req.guild_id.get(),
                        req.channel_id.get(),
                        e
                    );
                    self.set_last_error(req.guild_id, e);
                    return;
                }
                Err(_) => {
                    if let Some(handle) = next_in_flight {
                        handle.abort();
                    }
                    let err = format!(
                        "segment playback timed out after {:?}",
                        SEGMENT_PLAYBACK_TIMEOUT
                    );
                    warn!(
                        "{} (guild={}, channel={})",
                        err,
                        req.guild_id.get(),
                        req.channel_id.get(),
                    );
                    self.set_last_error(req.guild_id, err);
                    return;
                }
            }

            match next_in_flight {
                Some(next) => in_flight = next,
                None => break,
            }
        }

        self.clear_last_error(req.guild_id);
        debug!(
            "finished speak request guild={} channel={}",
            req.guild_id.get(),
            req.channel_id.get(),
        );
    }

    fn spawn_segment_synthesis(
        &self,
        segment: String,
        speaker: u32,
        speed_scale: Option<f32>,
        pitch_scale: Option<f32>,
        pan: Option<f32>,
    ) -> JoinHandle<Result<Vec<u8>, String>> {
        let this = self.clone();
        tokio::spawn(async move {
            let wav = this
                .synthesize(&segment, speaker, speed_scale, pitch_scale)
                .await?;
            apply_pan_to_wav(wav, pan)
        })
    }

    async fn ensure_core_initialized(&self) -> Result<Arc<CoreRuntime>, String> {
        let config = self.core_config.clone();
        let runtime = self
            .core_runtime
            .get_or_try_init(|| async move { CoreRuntime::new(&config).await.map(Arc::new) })
            .await?;

        Ok(Arc::clone(runtime))
    }

    async fn synthesize(
        &self,
        text: &str,
        speaker: u32,
        speed_scale: Option<f32>,
        pitch_scale: Option<f32>,
    ) -> Result<Vec<u8>, String> {
        let runtime = self.ensure_core_initialized().await?;
        runtime
            .synthesize(text, speaker, speed_scale, pitch_scale)
            .await
    }

    async fn play_wav_segment(
        &self,
        guild_id: GuildId,
        wav: Vec<u8>,
        parallel_mode: bool,
    ) -> Result<(), String> {
        let manager = self
            .songbird_manager()
            .ok_or_else(|| "Voice manager is not initialized".to_string())?;

        let call = manager
            .get(guild_id)
            .ok_or_else(|| "Not connected to a voice channel".to_string())?;

        // 生成WAVをそのままメモリ入力として再生し、FS書き込みを完全に回避する。
        let input: SongbirdInput = wav.into();

        if parallel_mode {
            let handle = {
                let mut handler = call.lock().await;
                handler.set_bitrate(SongbirdBitrate::Max);
                handler.play_input(input)
            };

            let _ = handle.make_playable_async().await;
            self.wait_track_end(handle).await;
        } else {
            let mut handler = call.lock().await;
            handler.set_bitrate(SongbirdBitrate::Max);
            let _ = handler.enqueue_input(input).await;
        }

        Ok(())
    }

    async fn wait_track_end(&self, handle: TrackHandle) {
        let mut consecutive_errors = 0usize;
        loop {
            match handle.get_info().await {
                Ok(state) if state.playing.is_done() => return,
                Ok(_) => {
                    consecutive_errors = 0;
                    sleep(TRACK_END_POLL_INTERVAL).await;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= TRACK_END_MAX_CONSECUTIVE_ERRORS {
                        debug!(
                            "track state polling failed repeatedly; continue pipeline as finished: {}",
                            e
                        );
                        return;
                    }
                    sleep(TRACK_END_POLL_INTERVAL).await;
                }
            }
        }
    }

    fn songbird_manager(&self) -> Option<Arc<Songbird>> {
        self.songbird.read().ok().and_then(|r| r.clone())
    }
}

fn apply_pan_to_wav(wav: Vec<u8>, pan: Option<f32>) -> Result<Vec<u8>, String> {
    let pan = pan.unwrap_or(0.0).clamp(-1.0, 1.0);
    if pan.abs() < 0.0001 {
        return Ok(wav);
    }

    let meta = parse_pcm_wav(&wav)?;

    if meta.audio_format != 1 || meta.bits_per_sample != 16 {
        return Err("pan processing supports only PCM16 WAV".to_string());
    }

    let left_gain = (1.0 - pan).clamp(0.0, 1.0);
    let right_gain = (1.0 + pan).clamp(0.0, 1.0);

    if meta.channels == 2 {
        let mut out = wav;
        let mut pos = meta.data_start;
        while pos + 3 < meta.data_end {
            let left = i16::from_le_bytes([out[pos], out[pos + 1]]);
            let right = i16::from_le_bytes([out[pos + 2], out[pos + 3]]);
            let left = scale_pcm16(left, left_gain);
            let right = scale_pcm16(right, right_gain);
            out[pos..pos + 2].copy_from_slice(&left.to_le_bytes());
            out[pos + 2..pos + 4].copy_from_slice(&right.to_le_bytes());
            pos += 4;
        }
        return Ok(out);
    }

    if meta.channels != 1 {
        return Err(format!(
            "pan processing supports only mono/stereo WAV (got {} ch)",
            meta.channels
        ));
    }

    let mut new_data = Vec::with_capacity((meta.data_end - meta.data_start) * 2);
    let mut pos = meta.data_start;
    while pos + 1 < meta.data_end {
        let sample = i16::from_le_bytes([wav[pos], wav[pos + 1]]);
        let left = scale_pcm16(sample, left_gain);
        let right = scale_pcm16(sample, right_gain);
        new_data.extend_from_slice(&left.to_le_bytes());
        new_data.extend_from_slice(&right.to_le_bytes());
        pos += 2;
    }

    let mut out = replace_wav_data(&wav, &meta, &new_data, "pan processing")?;

    write_u16_le(
        &mut out[meta.fmt_data_start + 2..meta.fmt_data_start + 4],
        2,
    );
    let block_align = 4u16;
    write_u16_le(
        &mut out[meta.fmt_data_start + 12..meta.fmt_data_start + 14],
        block_align,
    );
    let byte_rate = meta
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "WAV byte rate overflow".to_string())?;
    write_u32_le(
        &mut out[meta.fmt_data_start + 8..meta.fmt_data_start + 12],
        byte_rate,
    );

    Ok(out)
}

struct ParsedPcmWav {
    fmt_data_start: usize,
    data_chunk_start: usize,
    data_start: usize,
    data_end: usize,
    audio_format: u16,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
}

fn concatenate_wav_segments(segments: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
    let mut segments = segments.into_iter();
    let Some(first) = segments.next() else {
        return Err("No WAV segments to concatenate".to_string());
    };

    let first_meta = parse_pcm_wav(&first)?;
    let mut combined_data = Vec::new();
    combined_data.extend_from_slice(&first[first_meta.data_start..first_meta.data_end]);

    for wav in segments {
        let meta = parse_pcm_wav(&wav)?;
        if meta.audio_format != first_meta.audio_format
            || meta.channels != first_meta.channels
            || meta.sample_rate != first_meta.sample_rate
            || meta.bits_per_sample != first_meta.bits_per_sample
        {
            return Err("WAV segments use different audio formats".to_string());
        }

        combined_data.extend_from_slice(&wav[meta.data_start..meta.data_end]);
    }

    replace_wav_data(&first, &first_meta, &combined_data, "concatenation")
}

fn replace_wav_data(
    wav: &[u8],
    meta: &ParsedPcmWav,
    new_data: &[u8],
    operation: &str,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(
        wav.len()
            + new_data
                .len()
                .saturating_sub(meta.data_end - meta.data_start),
    );
    out.extend_from_slice(&wav[..meta.data_chunk_start + 8]);
    out.extend_from_slice(new_data);
    out.extend_from_slice(&wav[meta.data_end..]);

    let new_data_size = u32::try_from(new_data.len())
        .map_err(|_| format!("WAV data size is too large after {operation}"))?;
    write_u32_le(
        &mut out[meta.data_chunk_start + 4..meta.data_chunk_start + 8],
        new_data_size,
    );

    let riff_size = u32::try_from(out.len().saturating_sub(8))
        .map_err(|_| format!("WAV RIFF size is too large after {operation}"))?;
    write_u32_le(&mut out[4..8], riff_size);

    Ok(out)
}

fn parse_pcm_wav(wav: &[u8]) -> Result<ParsedPcmWav, String> {
    if wav.len() < 44 {
        return Err("generated WAV is too short".to_string());
    }
    if &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err("generated audio is not RIFF/WAVE".to_string());
    }

    let mut fmt_chunk_start = None;
    let mut data_chunk_start = None;
    let mut data_chunk_size = 0usize;

    let mut cursor = 12usize;
    while cursor + 8 <= wav.len() {
        let chunk_size = read_u32_le(&wav[cursor + 4..cursor + 8]) as usize;
        let chunk_data_start = cursor + 8;
        let chunk_data_end = chunk_data_start
            .checked_add(chunk_size)
            .ok_or_else(|| "WAV chunk size overflow".to_string())?;
        if chunk_data_end > wav.len() {
            return Err("WAV chunk extends past buffer".to_string());
        }

        match &wav[cursor..cursor + 4] {
            b"fmt " => fmt_chunk_start = Some(cursor),
            b"data" => {
                data_chunk_start = Some(cursor);
                data_chunk_size = chunk_size;
            }
            _ => {}
        }

        cursor = chunk_data_end + (chunk_size & 1);
    }

    let fmt_chunk_start = fmt_chunk_start.ok_or_else(|| "WAV fmt chunk not found".to_string())?;
    let data_chunk_start =
        data_chunk_start.ok_or_else(|| "WAV data chunk not found".to_string())?;

    if fmt_chunk_start > data_chunk_start {
        return Err("unsupported WAV chunk order".to_string());
    }

    let fmt_chunk_size = read_u32_le(&wav[fmt_chunk_start + 4..fmt_chunk_start + 8]) as usize;
    let fmt_data_start = fmt_chunk_start + 8;
    if fmt_chunk_size < 16 || fmt_data_start + fmt_chunk_size > wav.len() {
        return Err("invalid WAV fmt chunk".to_string());
    }

    let data_start = data_chunk_start + 8;
    let data_end = data_start
        .checked_add(data_chunk_size)
        .ok_or_else(|| "WAV data size overflow".to_string())?;
    if data_end > wav.len() {
        return Err("invalid WAV data chunk".to_string());
    }

    Ok(ParsedPcmWav {
        fmt_data_start,
        data_chunk_start,
        data_start,
        data_end,
        audio_format: read_u16_le(&wav[fmt_data_start..fmt_data_start + 2]),
        channels: read_u16_le(&wav[fmt_data_start + 2..fmt_data_start + 4]),
        sample_rate: read_u32_le(&wav[fmt_data_start + 4..fmt_data_start + 8]),
        bits_per_sample: read_u16_le(&wav[fmt_data_start + 14..fmt_data_start + 16]),
    })
}

fn read_u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn write_u16_le(bytes: &mut [u8], value: u16) {
    bytes.copy_from_slice(&value.to_le_bytes());
}

fn write_u32_le(bytes: &mut [u8], value: u32) {
    bytes.copy_from_slice(&value.to_le_bytes());
}

fn scale_pcm16(sample: i16, gain: f32) -> i16 {
    let scaled = (sample as f32) * gain;
    scaled.clamp(i16::MIN as f32, i16::MAX as f32).round() as i16
}

fn enforce_min_speed_for_long_text(text: &str, speed_scale: Option<f32>) -> Option<f32> {
    if text.chars().count() < LONG_TTS_TEXT_THRESHOLD_CHARS {
        return speed_scale;
    }

    Some(
        speed_scale
            .unwrap_or(LONG_TTS_TEXT_MIN_SPEED)
            .max(LONG_TTS_TEXT_MIN_SPEED),
    )
}

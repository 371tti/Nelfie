use serenity::all::{ChannelId, GuildId};

pub const VOICE_PARALLEL_COUNT_DEFAULT: usize = 1;
pub const VOICE_PARALLEL_COUNT_MAX: usize = 4;

pub(crate) fn normalize_parallel_count(parallel_count: usize) -> usize {
    parallel_count.clamp(VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX)
}

#[derive(Clone, Debug)]
pub struct VoiceCoreConfig {
    pub acceleration_mode: String,
    pub cpu_threads: u16,
    pub output_sampling_rate: u32,
    pub open_jtalk_dict_dir: String,
    pub vvm_dir: String,
    pub onnxruntime_filename: String,
}

#[derive(Clone, Debug)]
pub struct GuildVoiceConfig {
    pub text_channel_id: Option<ChannelId>,
    pub auto_read: bool,
    pub speaker: u32,
}

#[derive(Clone, Debug)]
pub struct SpeakOptions {
    pub speaker: Option<u32>,
    pub speed_scale: Option<f32>,
    pub pitch_scale: Option<f32>,
    pub pan: Option<f32>,
    pub channel_id: ChannelId,
    pub parallel_count: usize,
}

#[derive(Clone, Debug)]
pub(super) struct SpeakRequest {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub text: String,
    pub speaker: u32,
    pub speed_scale: Option<f32>,
    pub pitch_scale: Option<f32>,
    pub pan: Option<f32>,
    pub parallel_count: usize,
}

use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, UserId};

use crate::{
    llm::channel::{
        VOICE_DICTIONARY_MAX_ENTRIES, VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX,
    },
    llm::tool::LMTool,
    voice::{SpeakOptions, apply_tts_dictionary, voice_catalog},
};

pub struct VoicevoxTool;

impl VoicevoxTool {
    pub fn new() -> Self {
        Self {}
    }

    fn get_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Result<&'a str, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
    }

    fn get_bool_arg(args: &serde_json::Value, key: &str) -> Result<bool, String> {
        args.get(key)
            .and_then(|v| v.as_bool())
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))
    }

    fn get_optional_str_arg<'a>(args: &'a serde_json::Value, key: &'a str) -> Option<&'a str> {
        args.get(key).and_then(|v| v.as_str()).map(str::trim)
    }

    fn parse_id(raw: &str, key: &str) -> Result<u64, String> {
        raw.parse::<u64>()
            .map_err(|e| format!("Invalid '{key}': {e}"))
    }

    fn parse_id_value(raw: &Value, key: &str) -> Result<u64, String> {
        if let Some(value) = raw.as_u64() {
            return Ok(value);
        }

        if let Some(value) = raw.as_str() {
            return Self::parse_id(value, key);
        }

        Err(format!("Missing or invalid '{key}' parameter"))
    }

    fn get_guild_arg(args: &serde_json::Value) -> Result<(GuildId, String), String> {
        let guild_raw = Self::get_str_arg(args, "guild_id")?;
        let guild_id = GuildId::new(Self::parse_id(guild_raw, "guild_id")?);
        Ok((guild_id, guild_raw.to_string()))
    }

    fn get_optional_channel_arg(
        args: &serde_json::Value,
        key: &str,
    ) -> Result<Option<ChannelId>, String> {
        let Some(raw) = args.get(key) else {
            return Ok(None);
        };

        let id = Self::parse_id_value(raw, key)?;
        Ok(Some(ChannelId::new(id)))
    }

    fn get_optional_u32_setting(
        args: &serde_json::Value,
        key: &str,
    ) -> Result<Option<Option<u32>>, String> {
        let Some(raw) = args.get(key) else {
            return Ok(None);
        };

        if raw.is_null() {
            return Ok(Some(None));
        }

        let value = Self::parse_id_value(raw, key)?;
        let value = u32::try_from(value).map_err(|_| format!("'{key}' must be <= {}", u32::MAX))?;
        Ok(Some(Some(value)))
    }

    fn get_optional_f32_setting(
        args: &serde_json::Value,
        key: &str,
        min: f32,
        max: f32,
    ) -> Result<Option<Option<f32>>, String> {
        let Some(raw) = args.get(key) else {
            return Ok(None);
        };

        if raw.is_null() {
            return Ok(Some(None));
        }

        let value = if let Some(v) = raw.as_f64() {
            v as f32
        } else if let Some(v) = raw.as_str() {
            v.parse::<f32>()
                .map_err(|e| format!("Invalid '{key}': {e}"))?
        } else {
            return Err(format!("Missing or invalid '{key}' parameter"));
        };

        if !value.is_finite() {
            return Err(format!("'{key}' must be finite"));
        }
        if !(min..=max).contains(&value) {
            return Err(format!("'{key}' must be in range [{min}, {max}]"));
        }

        Ok(Some(Some(value)))
    }

    fn parse_u32(args: &serde_json::Value, key: &str) -> Result<u32, String> {
        let value = args
            .get(key)
            .ok_or_else(|| format!("Missing or invalid '{key}' parameter"))?;
        let value = Self::parse_id_value(value, key)?;

        u32::try_from(value).map_err(|_| format!("'{key}' must be <= {}", u32::MAX))
    }

    fn parse_limit_arg(args: &serde_json::Value, key: &str, default: usize, max: usize) -> usize {
        let value = args
            .get(key)
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(default);

        value.clamp(1, max)
    }

    fn parse_parallel_count_arg(
        args: &serde_json::Value,
        key: &str,
        default: usize,
    ) -> Result<usize, String> {
        let Some(raw) = args.get(key) else {
            return Ok(default);
        };

        let value = Self::parse_id_value(raw, key)?;
        let value = usize::try_from(value)
            .map_err(|_| format!("'{key}' is too large for this platform"))?;

        Ok(value.clamp(VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX))
    }
}

impl Default for VoicevoxTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LMTool for VoicevoxTool {
    fn name(&self) -> String {
        "voicevox-tool".to_string()
    }

    fn description(&self) -> String {
        "Control standalone VOICEVOX CORE + Discord VC reading (no external Engine server required). You can join/leave voice channels, speak text, configure auto-read and per-text-channel parallel count, update guild and user voice settings (including other users), and list VOICEVOX style IDs.".to_string()
    }

    fn json_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "description": "Operation to perform.",
                    "enum": [
                        "join_voice",
                        "leave_voice",
                        "speak_text",
                        "register_dictionary",
                        "set_auto_read",
                        "set_parallel_read",
                        "set_speaker",
                        "set_user_voice",
                        "get_user_voice",
                        "list_voice_styles",
                        "list_voice_speakers",
                        "status"
                    ]
                },
                "guild_id": {
                    "type": "string",
                    "description": "Discord guild ID as a decimal string."
                },
                "channel_id": {
                    "type": "string",
                    "description": "Discord channel ID as a decimal string. Required for join_voice."
                },
                "text_channel_id": {
                    "type": "string",
                    "description": "Text channel ID used for auto-read target."
                },
                "text": {
                    "type": "string",
                    "description": "Text to read aloud. Required for speak_text."
                },
                "speaker": {
                    "type": ["integer", "string", "null"],
                    "description": "VOICEVOX speaker ID (style ID). Null clears the setting for set_user_voice."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Enable or disable auto-read for set_auto_read."
                },
                "parallel_count": {
                    "type": ["integer", "string"],
                    "description": "Read parallel count for a text channel (1..4) used by set_parallel_read."
                },
                "user_id": {
                    "type": "string",
                    "description": "Discord user ID as a decimal string. Required for set_user_voice/get_user_voice."
                },
                "voice_speed_scale": {
                    "type": ["number", "string", "null"],
                    "description": "Voice speed scale for user voice settings (0.5 to 2.0). Null clears the value."
                },
                "voice_pitch_scale": {
                    "type": ["number", "string", "null"],
                    "description": "Voice pitch scale for user voice settings (-1.0 to 1.0). Null clears the value."
                },
                "voice_pan": {
                    "type": ["number", "string", "null"],
                    "description": "Voice pan for user voice settings (-1.0 to 1.0). Null clears the value."
                },
                "speaker_name": {
                    "type": "string",
                    "description": "Speaker name filter for list_voice_styles."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of items to return for list operations."
                },
                "dictionary_source": {
                    "type": "string",
                    "description": "Source phrase to match for TTS dictionary registration. Required for register_dictionary."
                },
                "dictionary_target": {
                    "type": "string",
                    "description": "Replacement phrase to read aloud for TTS dictionary registration. Required for register_dictionary."
                }
            },
            "required": ["operation"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ob_ctx: crate::app::context::NelfieContext,
    ) -> Result<String, String> {
        let operation = args
            .get("operation")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing or invalid 'operation' parameter".to_string())?;

        match operation {
            "list_voice_styles" => {
                let speaker_filter = Self::get_optional_str_arg(&args, "speaker_name")
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_string());
                let limit = Self::parse_limit_arg(&args, "limit", 128, 512);
                let filter_key = speaker_filter.as_ref().map(|value| value.to_lowercase());

                let mut total = 0usize;
                let mut styles = Vec::new();
                for entry in voice_catalog::entries() {
                    if let Some(filter_key) = &filter_key
                        && !entry.speaker_name.to_lowercase().contains(filter_key)
                    {
                        continue;
                    }

                    total += 1;
                    if styles.len() < limit {
                        styles.push(json!({
                            "speaker": entry.speaker_name,
                            "style": entry.style_name,
                            "style_id": entry.style_id,
                            "vvm_file": entry.vvm_file,
                        }));
                    }
                }

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "speaker_filter": speaker_filter,
                    "returned": styles.len(),
                    "total_matches": total,
                    "styles": styles,
                })
                .to_string())
            }
            "list_voice_speakers" => {
                let name_filter = Self::get_optional_str_arg(&args, "speaker_name")
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_lowercase());
                let limit = Self::parse_limit_arg(&args, "limit", 128, 512);

                let mut style_counts = std::collections::BTreeMap::<String, usize>::new();
                for entry in voice_catalog::entries() {
                    *style_counts.entry(entry.speaker_name.clone()).or_insert(0) += 1;
                }

                let mut total = 0usize;
                let mut speakers = Vec::new();
                for (speaker_name, style_count) in style_counts {
                    if let Some(name_filter) = &name_filter
                        && !speaker_name.to_lowercase().contains(name_filter)
                    {
                        continue;
                    }

                    total += 1;
                    if speakers.len() < limit {
                        speakers.push(json!({
                            "speaker": speaker_name,
                            "style_count": style_count,
                        }));
                    }
                }

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "returned": speakers.len(),
                    "total_matches": total,
                    "speakers": speakers,
                })
                .to_string())
            }
            "join_voice" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let channel_raw = args
                    .get("channel_id")
                    .ok_or_else(|| "Missing or invalid 'channel_id' parameter".to_string())?;
                let channel_id_num = Self::parse_id_value(channel_raw, "channel_id")?;
                let channel_id = ChannelId::new(channel_id_num);

                ob_ctx.voice_system.join_voice(guild_id, channel_id).await?;

                let enabled = args
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let text_channel = Self::get_optional_channel_arg(&args, "text_channel_id")?;

                if enabled {
                    let target = text_channel.ok_or_else(|| {
                        "'text_channel_id' is required when enabled=true".to_string()
                    })?;
                    ob_ctx.chat_contexts.set_voice_auto_read(target, true);
                    ob_ctx
                        .voice_system
                        .set_auto_read(guild_id, true, Some(target));
                } else if let Some(target) = text_channel {
                    ob_ctx.chat_contexts.set_voice_auto_read(target, false);
                    ob_ctx
                        .voice_system
                        .set_auto_read(guild_id, false, Some(target));
                }

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "voice_channel_id": channel_id_num.to_string(),
                    "auto_read": enabled,
                    "text_channel_id": text_channel.map(|id| id.get().to_string()),
                })
                .to_string())
            }
            "leave_voice" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                ob_ctx.voice_system.leave_voice(guild_id).await?;
                ob_ctx.voice_system.set_auto_read(guild_id, false, None);

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                })
                .to_string())
            }
            "speak_text" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let text = Self::get_str_arg(&args, "text")?;
                let text_channel = Self::get_optional_channel_arg(&args, "text_channel_id")?;
                let speaker: Option<u32> =
                    Self::get_optional_u32_setting(&args, "speaker")?.unwrap_or_default();
                let cfg = ob_ctx.voice_system.config(guild_id);
                let speak_channel = text_channel.or(cfg.text_channel_id).ok_or_else(|| {
                    "'text_channel_id' is required when no auto-read target channel is configured"
                        .to_string()
                })?;
                let parallel_count = ob_ctx.chat_contexts.voice_parallel_count(speak_channel);
                ob_ctx
                    .voice_system
                    .set_channel_parallel_count(speak_channel, parallel_count);

                let dictionary = ob_ctx.chat_contexts.voice_dictionary_entries(speak_channel);
                let processed_text = apply_tts_dictionary(text, &dictionary);
                let queued_text_length = processed_text.chars().count();

                ob_ctx
                    .voice_system
                    .speak(
                        guild_id,
                        processed_text,
                        SpeakOptions {
                            speaker,
                            speed_scale: None,
                            pitch_scale: None,
                            pan: None,
                            channel_id: speak_channel,
                            parallel_count,
                        },
                    )
                    .await?;

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "requested_text_length": text.chars().count(),
                    "queued_text_length": queued_text_length,
                    "text_channel_id": speak_channel.get().to_string(),
                    "parallel_count": parallel_count,
                    "speaker": speaker,
                })
                .to_string())
            }
            "register_dictionary" => {
                let (_guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let text_channel_raw = args
                    .get("text_channel_id")
                    .ok_or_else(|| "Missing or invalid 'text_channel_id' parameter".to_string())?;
                let text_channel_id = Self::parse_id_value(text_channel_raw, "text_channel_id")?;
                let text_channel = ChannelId::new(text_channel_id);
                let source = Self::get_str_arg(&args, "dictionary_source")?;
                let target = Self::get_str_arg(&args, "dictionary_target")?;

                let (count, updated) = ob_ctx.chat_contexts.set_voice_dictionary_entry(
                    text_channel,
                    source.to_string(),
                    target.to_string(),
                )?;

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "text_channel_id": text_channel_id.to_string(),
                    "dictionary_source": source.trim(),
                    "dictionary_target": target.trim(),
                    "updated": updated,
                    "entries": count,
                    "entries_limit": VOICE_DICTIONARY_MAX_ENTRIES,
                })
                .to_string())
            }
            "set_auto_read" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let enabled = Self::get_bool_arg(&args, "enabled")?;
                let text_channel = Self::get_optional_channel_arg(&args, "text_channel_id")?;

                if enabled {
                    let target = text_channel.ok_or_else(|| {
                        "'text_channel_id' is required when enabled=true".to_string()
                    })?;
                    ob_ctx.chat_contexts.set_voice_auto_read(target, true);
                    ob_ctx
                        .voice_system
                        .set_auto_read(guild_id, true, Some(target));
                } else if let Some(target) = text_channel {
                    ob_ctx.chat_contexts.set_voice_auto_read(target, false);
                    ob_ctx
                        .voice_system
                        .set_auto_read(guild_id, false, Some(target));
                }

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "enabled": enabled,
                    "text_channel_id": text_channel.map(|id| id.get().to_string()),
                })
                .to_string())
            }
            "set_parallel_read" => {
                let (_guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let text_channel = Self::get_optional_channel_arg(&args, "text_channel_id")?
                    .ok_or_else(|| {
                        "'text_channel_id' is required for set_parallel_read".to_string()
                    })?;

                let parallel_count = if args.get("parallel_count").is_some() {
                    Self::parse_parallel_count_arg(
                        &args,
                        "parallel_count",
                        VOICE_PARALLEL_COUNT_DEFAULT,
                    )?
                } else {
                    let enabled = args
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if enabled {
                        2
                    } else {
                        VOICE_PARALLEL_COUNT_DEFAULT
                    }
                };

                let parallel_count = ob_ctx
                    .chat_contexts
                    .set_voice_parallel_count(text_channel, parallel_count);
                ob_ctx
                    .voice_system
                    .set_channel_parallel_count(text_channel, parallel_count);

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "text_channel_id": text_channel.get().to_string(),
                    "parallel_count": parallel_count,
                    "parallel_count_range": format!("{}..={}", VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX),
                    "sequential_queue_limit": ob_ctx.voice_system.sequential_queue_capacity(),
                })
                .to_string())
            }
            "set_speaker" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let speaker = Self::parse_u32(&args, "speaker")?;
                ob_ctx.voice_system.set_speaker(guild_id, speaker);

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "speaker": speaker,
                })
                .to_string())
            }
            "set_user_voice" => {
                let (_guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let user_raw = Self::get_str_arg(&args, "user_id")?;
                let user_id = UserId::new(Self::parse_id(user_raw, "user_id")?);

                let speaker_update = Self::get_optional_u32_setting(&args, "speaker")?;
                let speed_update =
                    Self::get_optional_f32_setting(&args, "voice_speed_scale", 0.5, 2.0)?;
                let pitch_update =
                    Self::get_optional_f32_setting(&args, "voice_pitch_scale", -1.0, 1.0)?;
                let pan_update = Self::get_optional_f32_setting(&args, "voice_pan", -1.0, 1.0)?;

                if speaker_update.is_none()
                    && speed_update.is_none()
                    && pitch_update.is_none()
                    && pan_update.is_none()
                {
                    return Err(
                        "Specify at least one of: speaker, voice_speed_scale, voice_pitch_scale, voice_pan."
                            .to_string(),
                    );
                }

                if let Some(value) = speaker_update {
                    ob_ctx.user_contexts.set_voice_speaker(user_id, value);
                }
                if let Some(value) = speed_update {
                    ob_ctx.user_contexts.set_voice_speed_scale(user_id, value);
                }
                if let Some(value) = pitch_update {
                    ob_ctx.user_contexts.set_voice_pitch_scale(user_id, value);
                }
                if let Some(value) = pan_update {
                    ob_ctx.user_contexts.set_voice_pan(user_id, value);
                }

                let updated = ob_ctx.user_contexts.get_or_create(user_id);

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "user_id": user_raw,
                    "voice": {
                        "speaker": updated.voice_speaker,
                        "speaker_name": updated
                            .voice_speaker
                            .and_then(voice_catalog::speaker_name_for_id),
                        "style_name": updated
                            .voice_speaker
                            .and_then(voice_catalog::style_name_for_id),
                        "voice_speed_scale": updated.voice_speed_scale,
                        "voice_pitch_scale": updated.voice_pitch_scale,
                        "voice_pan": updated.voice_pan,
                    },
                })
                .to_string())
            }
            "get_user_voice" => {
                let (_guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let user_raw = Self::get_str_arg(&args, "user_id")?;
                let user_id = UserId::new(Self::parse_id(user_raw, "user_id")?);
                let current = ob_ctx.user_contexts.get_or_create(user_id);

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "user_id": user_raw,
                    "voice": {
                        "speaker": current.voice_speaker,
                        "speaker_name": current
                            .voice_speaker
                            .and_then(voice_catalog::speaker_name_for_id),
                        "style_name": current
                            .voice_speaker
                            .and_then(voice_catalog::style_name_for_id),
                        "voice_speed_scale": current.voice_speed_scale,
                        "voice_pitch_scale": current.voice_pitch_scale,
                        "voice_pan": current.voice_pan,
                    },
                })
                .to_string())
            }
            "status" => {
                let (guild_id, guild_id_raw) = Self::get_guild_arg(&args)?;
                let cfg = ob_ctx.voice_system.config(guild_id);
                let current_vc = ob_ctx
                    .voice_system
                    .current_voice_channel_raw(guild_id)
                    .await;
                let last_error = ob_ctx.voice_system.last_error(guild_id);
                let text_channel = Self::get_optional_channel_arg(&args, "text_channel_id")?;
                let effective_text_channel = text_channel.or(cfg.text_channel_id);
                let auto_read_for_channel =
                    effective_text_channel.map(|id| ob_ctx.chat_contexts.is_voice_auto_read(id));
                let dictionary_entries_for_channel = effective_text_channel
                    .map(|id| ob_ctx.chat_contexts.voice_dictionary_count(id));
                let parallel_count_for_channel =
                    effective_text_channel.map(|id| ob_ctx.chat_contexts.voice_parallel_count(id));

                Ok(json!({
                    "status": "ok",
                    "operation": operation,
                    "guild_id": guild_id_raw,
                    "connected_voice_channel_id": current_vc.map(|id| id.to_string()),
                    "auto_read_for_text_channel": auto_read_for_channel,
                    "dictionary_entries_for_text_channel": dictionary_entries_for_channel,
                    "dictionary_entries_limit": VOICE_DICTIONARY_MAX_ENTRIES,
                    "text_channel_id": text_channel.map(|id| id.get().to_string()),
                    "effective_text_channel_id": effective_text_channel.map(|id| id.get().to_string()),
                    "default_speaker": cfg.speaker,
                    "parallel_count_for_text_channel": parallel_count_for_channel,
                    "parallel_count_range": format!("{}..={}", VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX),
                    "sequential_queue_limit": ob_ctx.voice_system.sequential_queue_capacity(),
                    "voice_mode": "voicevox_core_standalone",
                    "core_runtime": ob_ctx.voice_system.core_summary(),
                    "last_error": last_error,
                })
                .to_string())
            }
            other => Err(format!(
                "Unsupported 'operation': {other}. Use one of: join_voice, leave_voice, speak_text, register_dictionary, set_auto_read, set_parallel_read, set_speaker, set_user_voice, get_user_voice, list_voice_styles, list_voice_speakers, status."
            )),
        }
    }
}

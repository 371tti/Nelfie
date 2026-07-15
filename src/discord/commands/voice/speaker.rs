use serenity::all::{AutocompleteChoice, CreateEmbed, ResolvedValue};

use crate::{
    llm::channel::{
        VOICE_DICTIONARY_MAX_ENTRIES, VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX,
    },
    voice::voice_catalog,
};

use super::super::shared::{Context, Error};
use super::support::{
    format_optional_voice_value, format_voice_style_label, require_vc_guild, send_vc_embed,
    send_vc_error, speak_vc_system_message,
};

/// VOICEVOXの話者とスタイルを設定します（ユーザーごと）
#[poise::command(slash_command, prefix_command)]
pub async fn vc_speaker(
    ctx: Context<'_>,
    #[description = "VOICEVOX話者名"]
    #[autocomplete = "autocomplete_vc_speaker"]
    speaker: String,
    #[description = "スタイル名"]
    #[autocomplete = "autocomplete_vc_style"]
    style: String,
    #[description = "話速 (0.5〜2.0, 省略時は現状維持)"] speed: Option<f32>,
    #[description = "音高 (-1.0〜1.0, 省略時は現状維持)"] pitch: Option<f32>,
    #[description = "左右pan (-1.0=左, 0.0=中央, 1.0=右, 省略時は現状維持)"] pan: Option<f32>,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_speaker").await? else {
        return Ok(());
    };

    let Some(style_id) = voice_catalog::find_style_id(&speaker, &style) else {
        let styles = voice_catalog::styles_for_speaker(&speaker);
        if styles.is_empty() {
            let speaker_preview = voice_catalog::speaker_names()
                .into_iter()
                .take(20)
                .collect::<Vec<_>>()
                .join(", ");
            send_vc_error(
                &ctx,
                format!(
                    "話者 '{}' は見つかりません。候補（先頭20件）: {}",
                    speaker, speaker_preview
                ),
            )
            .await?;
            return Ok(());
        }

        send_vc_error(
            &ctx,
            format!(
                "話者 '{}' にスタイル '{}' はありません。候補: {}",
                speaker,
                style,
                styles.join(", ")
            ),
        )
        .await?;
        return Ok(());
    };

    let speed = match speed {
        Some(v) if !(0.5..=2.0).contains(&v) => {
            send_vc_error(&ctx, "speed は 0.5〜2.0 の範囲で指定してください。").await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let pitch = match pitch {
        Some(v) if !(-1.0..=1.0).contains(&v) => {
            send_vc_error(&ctx, "pitch は -1.0〜1.0 の範囲で指定してください。").await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let pan = match pan {
        Some(v) if !(-1.0..=1.0).contains(&v) => {
            send_vc_error(&ctx, "pan は -1.0〜1.0 の範囲で指定してください。").await?;
            return Ok(());
        }
        Some(v) => Some(v),
        None => None,
    };

    let user_id = ctx.author().id;
    ctx.data()
        .user_contexts
        .set_voice_speaker(user_id, Some(style_id));
    if let Some(speed) = speed {
        ctx.data()
            .user_contexts
            .set_voice_speed_scale(user_id, Some(speed));
    }
    if let Some(pitch) = pitch {
        ctx.data()
            .user_contexts
            .set_voice_pitch_scale(user_id, Some(pitch));
    }
    if let Some(pan) = pan {
        ctx.data().user_contexts.set_voice_pan(user_id, Some(pan));
    }

    let speed_text = speed
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());
    let pitch_text = pitch
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());
    let pan_text = pan
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "(unchanged)".to_string());

    let embed = CreateEmbed::new()
        .title("VC話者設定")
        .description("話者設定を更新しました。")
        .field(
            "speaker",
            format!("VOICEVOX:{} / {} (id={})", speaker, style, style_id),
            false,
        )
        .field("speed", speed_text, true)
        .field("pitch", pitch_text, true)
        .field("pan", pan_text, true);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(&ctx, guild_id, "話者設定を更新しました").await;

    Ok(())
}

async fn autocomplete_vc_speaker(_ctx: Context<'_>, partial: &str) -> Vec<String> {
    voice_catalog::suggest_speakers(partial, 25)
}

fn selected_vc_speaker_from_ctx(ctx: Context<'_>) -> Option<String> {
    let poise::Context::Application(app_ctx) = ctx else {
        return None;
    };

    app_ctx.args.iter().find_map(|option| {
        if option.name != "speaker" {
            return None;
        }

        match option.value {
            ResolvedValue::String(value) => Some(value.to_string()),
            ResolvedValue::Autocomplete { value, .. } => Some(value.to_string()),
            _ => None,
        }
    })
}

async fn autocomplete_vc_style(ctx: Context<'_>, partial: &str) -> Vec<AutocompleteChoice> {
    let speaker = selected_vc_speaker_from_ctx(ctx);
    voice_catalog::suggest_styles(partial, speaker.as_deref(), 25)
        .into_iter()
        .map(|entry| {
            AutocompleteChoice::new(
                format!(
                    "{} / {} (id={}, {})",
                    entry.speaker_name, entry.style_name, entry.style_id, entry.vvm_file
                ),
                entry.style_name,
            )
        })
        .collect::<Vec<_>>()
}

/// VOICEVOXの話者設定と状態を取得します（ユーザーごと）
#[poise::command(slash_command, prefix_command)]
pub async fn vc_status(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_status").await? else {
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();
    let guild_voice_cfg = ob_ctx.voice_system.config(guild_id);
    let parallel_count = ob_ctx.chat_contexts.voice_parallel_count(channel_id);
    let voice_channel = ob_ctx
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await;
    let auto_read = ob_ctx.chat_contexts.is_voice_auto_read(channel_id);
    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);
    let dict_entries = ob_ctx.chat_contexts.voice_dictionary_count(channel_id);
    let user_dict_entries = ob_ctx.user_contexts.voice_dictionary_count(ctx.author().id);
    let user_voice = ob_ctx.user_contexts.get_or_create(ctx.author().id);
    let user_speaker = user_voice.voice_speaker;
    let speaker_text = user_speaker
        .map(format_voice_style_label)
        .unwrap_or_else(|| format!("{} (guild default)", guild_voice_cfg.speaker));
    let speed_text = format_optional_voice_value(user_voice.voice_speed_scale, "1.00 (default)");
    let pitch_text = format_optional_voice_value(user_voice.voice_pitch_scale, "0.00 (default)");
    let pan_text = format_optional_voice_value(user_voice.voice_pan, "0.00 (default)");

    let current_vc = voice_channel
        .map(|id| format!("<#{}>", id))
        .unwrap_or_else(|| "(not connected)".to_string());
    let last_error = ob_ctx
        .voice_system
        .last_error(guild_id)
        .unwrap_or_else(|| "(none)".to_string());

    let embed = CreateEmbed::new()
        .title("VCステータス")
        .field("connected", current_vc, true)
        .field("auto_read(this_channel)", auto_read.to_string(), true)
        .field("system_read(this_channel)", system_read.to_string(), true)
        .field(
            "parallel_count(this_channel)",
            parallel_count.to_string(),
            true,
        )
        .field(
            "parallel_count_range",
            format!(
                "{}..={}",
                VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX
            ),
            true,
        )
        .field(
            "mode(this_channel)",
            if parallel_count > 1 {
                format!("parallel(count={parallel_count})")
            } else {
                "sequential".to_string()
            },
            true,
        )
        .field(
            "sequential_queue_limit",
            ob_ctx.voice_system.sequential_queue_capacity().to_string(),
            true,
        )
        .field(
            "tts_dict_entries(this_channel)",
            format!("{dict_entries}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field(
            "tts_dict_entries(user)",
            format!("{user_dict_entries}/{VOICE_DICTIONARY_MAX_ENTRIES}"),
            true,
        )
        .field(
            "speaker(guild default)",
            guild_voice_cfg.speaker.to_string(),
            true,
        )
        .field("speaker(user)", speaker_text, false)
        .field(
            "speed/pitch/pan(user)",
            format!("{speed_text} / {pitch_text} / {pan_text}"),
            false,
        )
        .field("voicevox_core", ob_ctx.voice_system.core_summary(), false)
        .field(
            "acceleration/cpu_threads/load_all_models",
            format!(
                "{} / {} / {}",
                ob_ctx.config.voicevox_core_acceleration,
                ob_ctx.config.voicevox_core_cpu_threads,
                ob_ctx.config.voicevox_core_load_all_models
            ),
            false,
        )
        .field("last_error", last_error, false);
    send_vc_embed(&ctx, embed).await?;
    speak_vc_system_message(&ctx, guild_id, "VCステータスを表示しました").await;

    Ok(())
}

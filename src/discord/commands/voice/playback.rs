use poise::CreateReply;
use serenity::all::{CreateAttachment, CreateEmbed};

use crate::{
    llm::channel::{VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX},
    voice::SpeakOptions,
};

use super::super::shared::{Context, Error, preview_text};
use super::support::{
    apply_vc_dictionaries_for_ctx, build_vc_config_voice_message, build_vc_mode_label,
    find_author_voice_channel, format_voice_style_label, require_vc_guild, send_vc_embed,
    send_vc_embed_or_reply, send_vc_error, speak_vc_system_message, vc_error_embed,
};

/// VCに接続します(VC関連の機能が有効になります)
#[poise::command(slash_command, prefix_command)]
pub async fn vc_join(
    ctx: Context<'_>,
    #[description = "Enable auto-read in this text channel (default: true)"] auto_read: Option<
        bool,
    >,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_join").await? else {
        return Ok(());
    };

    let Some(voice_channel) = find_author_voice_channel(&ctx) else {
        send_vc_error(&ctx, "先にボイスチャンネルへ参加してください。").await?;
        return Ok(());
    };

    let ob_ctx = ctx.data();
    ob_ctx
        .voice_system
        .join_voice(guild_id, voice_channel)
        .await?;

    let auto_read = auto_read.unwrap_or(true);
    ob_ctx
        .chat_contexts
        .set_voice_auto_read(ctx.channel_id(), auto_read);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, auto_read, Some(ctx.channel_id()));

    let system_read = ob_ctx.chat_contexts.is_voice_system_read(ctx.channel_id());

    let embed = CreateEmbed::new()
        .title("VC接続")
        .description(format!("VC <#{}> に接続しました。", voice_channel.get()))
        .field(
            "auto_read",
            format!(
                "{}（対象テキストチャンネル: <#{}>）",
                auto_read,
                ctx.channel_id().get()
            ),
            false,
        )
        .field("system_read", system_read.to_string(), true)
        .field("TTS", "VOICEVOX", true)
        .field("VVM", "voicevox_vvm", false);

    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!(
            "ボイスチャンネルに接続しました。自動読み上げは{}です。",
            if auto_read { "有効" } else { "無効" }
        ),
    )
    .await;

    Ok(())
}

/// VCから切断します(VC関連の機能が無効になります)
#[poise::command(slash_command, prefix_command)]
pub async fn vc_leave(ctx: Context<'_>) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_leave").await? else {
        return Ok(());
    };

    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();
    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);

    if system_read {
        speak_vc_system_message(&ctx, guild_id, "ボイスチャンネルから切断します。").await;
    }

    ob_ctx.voice_system.leave_voice(guild_id).await?;
    ob_ctx.chat_contexts.set_voice_auto_read(channel_id, false);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, false, Some(channel_id));

    let embed = CreateEmbed::new()
        .title("VC切断")
        .description("VCから切断しました。")
        .field("auto_read", "false（このチャンネル）", false)
        .field("system_read", system_read.to_string(), true);

    send_vc_embed(&ctx, embed).await?;
    Ok(())
}

/// テキストをVCで読み上げます
#[poise::command(slash_command, prefix_command)]
pub async fn vc_say(
    ctx: Context<'_>,
    #[description = "Text to read in VC"]
    #[rest]
    text: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_say").await? else {
        return Ok(());
    };

    if text.trim().is_empty() {
        send_vc_error(&ctx, "読み上げるテキストが空です。").await?;
        return Ok(());
    }

    let text = apply_vc_dictionaries_for_ctx(&ctx, &text);
    let preview = preview_text(&text, 120);
    let channel_id = ctx.channel_id();
    let parallel_count = ctx.data().chat_contexts.voice_parallel_count(channel_id);

    let speaker = ctx.data().user_contexts.get_or_create(ctx.author().id);

    if let Err(e) = ctx
        .data()
        .voice_system
        .speak(
            guild_id,
            text,
            SpeakOptions {
                speaker: speaker.voice_speaker,
                speed_scale: speaker.voice_speed_scale,
                pitch_scale: speaker.voice_pitch_scale,
                pan: speaker.voice_pan,
                channel_id,
                parallel_count,
            },
        )
        .await
    {
        send_vc_error(&ctx, format!("読み上げキューへの追加に失敗しました: {e}")).await?;
        return Ok(());
    }

    let embed = CreateEmbed::new()
        .title("VC読み上げ")
        .description(if parallel_count > 1 {
            "読み上げジョブを追加しました（並列再生）。"
        } else {
            "読み上げキューに追加しました。"
        })
        .field("parallel_count", parallel_count.to_string(), true)
        .field("text", preview, false);
    send_vc_embed(&ctx, embed).await?;
    Ok(())
}

/// 現在の話者設定で音声ファイル（WAV）を生成して送信します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_download(
    ctx: Context<'_>,
    #[description = "Text to synthesize and download as WAV"]
    #[rest]
    text: String,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_download").await? else {
        return Ok(());
    };

    if text.trim().is_empty() {
        send_vc_error(&ctx, "生成するテキストが空です。").await?;
        return Ok(());
    }

    if let poise::Context::Application(app_ctx) = &ctx {
        app_ctx.defer().await?;
    }

    let ob_ctx = ctx.data();

    let text = apply_vc_dictionaries_for_ctx(&ctx, &text);
    let preview = preview_text(&text, 120);

    let user_voice = ob_ctx.user_contexts.get_or_create(ctx.author().id);
    let speaker_id = user_voice
        .voice_speaker
        .unwrap_or_else(|| ob_ctx.voice_system.config(guild_id).speaker);

    let wav = match ob_ctx
        .voice_system
        .synthesize_wav(
            text,
            speaker_id,
            user_voice.voice_speed_scale,
            user_voice.voice_pitch_scale,
            user_voice.voice_pan,
        )
        .await
    {
        Ok(wav) => wav,
        Err(e) => {
            send_vc_embed_or_reply(
                &ctx,
                vc_error_embed(format!("音声ファイル生成に失敗しました: {e}")),
            )
            .await?;
            return Ok(());
        }
    };

    let attachment = CreateAttachment::bytes(wav, "nelfie_tts.wav");
    let embed = CreateEmbed::new()
        .title("VC音声ファイル生成")
        .description("現在の設定でWAVファイルを生成しました。")
        .field("speaker", format_voice_style_label(speaker_id), false)
        .field("text", preview, false);

    ctx.send(CreateReply::default().embed(embed).attachment(attachment))
        .await?;

    Ok(())
}

/// VC関連設定（システム読み上げ / 自動読み上げ / 並列数）を更新します
#[poise::command(slash_command, prefix_command)]
pub async fn vc_config(
    ctx: Context<'_>,
    #[description = "Enable system read for VC command and join/leave announcements (None: keep current)"]
    system_read: Option<bool>,
    #[description = "Enable auto-read for this text channel (None: keep current)"]
    auto_read: Option<bool>,
    #[description = "Read parallel count for this text channel (1..4, None: keep current)"]
    parallel_count: Option<u8>,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_config").await? else {
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let ob_ctx = ctx.data();
    let changed = system_read.is_some() || auto_read.is_some() || parallel_count.is_some();

    let current_system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);
    let current_auto_read = ob_ctx.chat_contexts.is_voice_auto_read(channel_id);
    let next_system_read = system_read.unwrap_or(current_system_read);
    let next_auto_read = auto_read.unwrap_or(current_auto_read);

    if let Some(value) = auto_read {
        ob_ctx.chat_contexts.set_voice_auto_read(channel_id, value);
        ob_ctx
            .voice_system
            .set_auto_read(guild_id, value, Some(channel_id));
    }

    let parallel_count = match parallel_count {
        Some(value) => {
            let value = usize::from(value);
            if !(VOICE_PARALLEL_COUNT_DEFAULT..=VOICE_PARALLEL_COUNT_MAX).contains(&value) {
                send_vc_error(
                    &ctx,
                    format!(
                        "parallel_count は {}〜{} の範囲で指定してください。",
                        VOICE_PARALLEL_COUNT_DEFAULT, VOICE_PARALLEL_COUNT_MAX
                    ),
                )
                .await?;
                return Ok(());
            }

            ob_ctx
                .chat_contexts
                .set_voice_parallel_count(channel_id, value)
        }
        None => ob_ctx.chat_contexts.voice_parallel_count(channel_id),
    };
    ob_ctx
        .voice_system
        .set_channel_parallel_count(channel_id, parallel_count);

    if system_read.is_some() {
        ob_ctx
            .chat_contexts
            .set_voice_system_read(channel_id, next_system_read);
    }

    let system_read = next_system_read;
    let auto_read = next_auto_read;

    let queue_mode = build_vc_mode_label(
        parallel_count,
        ob_ctx.voice_system.sequential_queue_capacity(),
    );

    let embed = CreateEmbed::new()
        .title("VC設定")
        .description(if changed {
            "VC関連設定を更新しました。"
        } else {
            "現在のVC関連設定です。"
        })
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field("system_read", system_read.to_string(), true)
        .field("auto_read", auto_read.to_string(), true)
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
        .field("mode", queue_mode, false);
    send_vc_embed(&ctx, embed).await?;

    if system_read {
        speak_vc_system_message(
            &ctx,
            guild_id,
            build_vc_config_voice_message(changed, system_read, auto_read, parallel_count),
        )
        .await;
    }

    Ok(())
}

/// このテキストチャンネルでの自動読み上げを有効/無効にします
#[poise::command(slash_command, prefix_command)]
pub async fn vc_autoread(
    ctx: Context<'_>,
    #[description = "Enable auto-read for this text channel"] enabled: bool,
) -> Result<(), Error> {
    let Some(guild_id) = require_vc_guild(&ctx, "vc_autoread").await? else {
        return Ok(());
    };

    let channel_id = ctx.channel_id();
    let ob_ctx = ctx.data();

    ob_ctx
        .chat_contexts
        .set_voice_auto_read(channel_id, enabled);
    ob_ctx
        .voice_system
        .set_auto_read(guild_id, enabled, Some(channel_id));

    let system_read = ob_ctx.chat_contexts.is_voice_system_read(channel_id);

    let embed = CreateEmbed::new()
        .title("VC自動読み上げ設定")
        .description("このチャンネルの自動読み上げ設定を更新しました。")
        .field("channel", format!("<#{}>", channel_id.get()), true)
        .field("auto_read", enabled.to_string(), true)
        .field("system_read", system_read.to_string(), true);
    send_vc_embed(&ctx, embed).await?;

    speak_vc_system_message(
        &ctx,
        guild_id,
        format!(
            "このチャンネルの自動読み上げを{}に設定しました。",
            if enabled { "有効" } else { "無効" }
        ),
    )
    .await;

    Ok(())
}

use log::warn;
use poise::CreateReply;
use serenity::all::{
    ChannelId, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMessage, GuildId, InteractionResponseFlags, MessageFlags,
};

use crate::voice::{SpeakOptions, apply_tts_dictionaries, voice_catalog};

use super::super::shared::{Context, Error};

pub(super) fn find_author_voice_channel(ctx: &Context<'_>) -> Option<ChannelId> {
    let guild = ctx.guild()?;
    guild
        .voice_states
        .get(&ctx.author().id)
        .and_then(|state| state.channel_id)
}

pub(super) async fn require_vc_guild(
    ctx: &Context<'_>,
    command_name: &str,
) -> Result<Option<GuildId>, Error> {
    let Some(guild_id) = ctx.guild_id() else {
        send_vc_error(
            ctx,
            format!("{command_name} はサーバーチャンネル内でのみ使用できます。"),
        )
        .await?;
        return Ok(None);
    };

    Ok(Some(guild_id))
}

pub(super) fn apply_vc_dictionaries_for_ctx(ctx: &Context<'_>, text: &str) -> String {
    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();
    let user_id = ctx.author().id;
    let guild_dictionary = ob_ctx.chat_contexts.voice_dictionary_entries(channel_id);
    let user_dictionary = ob_ctx.user_contexts.voice_dictionary_entries(user_id);

    apply_tts_dictionaries(text, &guild_dictionary, &user_dictionary)
}

pub(super) async fn send_vc_embed(ctx: &Context<'_>, embed: CreateEmbed) -> Result<(), Error> {
    match ctx {
        poise::Context::Application(app_ctx) => {
            app_ctx
                .interaction
                .create_response(
                    app_ctx.serenity_context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(embed)
                            .flags(InteractionResponseFlags::SUPPRESS_NOTIFICATIONS),
                    ),
                )
                .await?;
            app_ctx
                .has_sent_initial_response
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        poise::Context::Prefix(prefix_ctx) => {
            prefix_ctx
                .msg
                .channel_id
                .send_message(
                    prefix_ctx.serenity_context,
                    CreateMessage::new()
                        .embed(embed)
                        .flags(MessageFlags::SUPPRESS_NOTIFICATIONS),
                )
                .await?;
        }
    }

    Ok(())
}

pub(super) async fn send_vc_embed_or_reply(
    ctx: &Context<'_>,
    embed: CreateEmbed,
) -> Result<(), Error> {
    match ctx {
        poise::Context::Application(_) => {
            ctx.send(CreateReply::default().embed(embed)).await?;
        }
        poise::Context::Prefix(_) => {
            send_vc_embed(ctx, embed).await?;
        }
    }

    Ok(())
}

pub(super) async fn send_vc_error(
    ctx: &Context<'_>,
    message: impl Into<String>,
) -> Result<(), Error> {
    send_vc_embed(ctx, vc_error_embed(message.into())).await
}

pub(super) fn vc_error_embed(message: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new().title("VCエラー").description(message)
}

pub(super) fn format_voice_style_label(style_id: u32) -> String {
    let speaker_name =
        voice_catalog::speaker_name_for_id(style_id).unwrap_or_else(|| "(unknown)".to_string());
    let style_name =
        voice_catalog::style_name_for_id(style_id).unwrap_or_else(|| "(unknown)".to_string());

    format!("{} / {} ({})", speaker_name, style_name, style_id)
}

pub(super) fn format_optional_voice_value(value: Option<f32>, fallback: &str) -> String {
    value
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| fallback.to_string())
}

pub(super) async fn speak_vc_system_message(
    ctx: &Context<'_>,
    guild_id: GuildId,
    text: impl Into<String>,
) {
    let ob_ctx = ctx.data();
    let channel_id = ctx.channel_id();

    if !ob_ctx.chat_contexts.is_voice_system_read(channel_id) {
        return;
    }

    let text = apply_vc_dictionaries_for_ctx(ctx, &text.into());
    let user_voice = ob_ctx.user_contexts.get_or_create(ctx.author().id);
    let parallel_count = ob_ctx.chat_contexts.voice_parallel_count(channel_id);

    if let Err(e) = ob_ctx
        .voice_system
        .speak(
            guild_id,
            text,
            SpeakOptions {
                speaker: user_voice.voice_speaker,
                speed_scale: user_voice.voice_speed_scale,
                pitch_scale: user_voice.voice_pitch_scale,
                pan: user_voice.voice_pan,
                channel_id,
                parallel_count,
            },
        )
        .await
    {
        warn!("failed to enqueue VC system message: {}", e);
    }
}

pub(super) fn bool_enabled_label(value: bool) -> &'static str {
    if value { "有効" } else { "無効" }
}

pub(super) fn build_vc_mode_label(parallel_count: usize, sequential_capacity: usize) -> String {
    if parallel_count > 1 {
        format!("parallel(count={parallel_count})")
    } else {
        format!("sequential(queue <= {sequential_capacity})")
    }
}

pub(super) fn build_vc_config_voice_message(
    is_updating: bool,
    system_read: bool,
    auto_read: bool,
    parallel_count: usize,
) -> String {
    let action = if is_updating {
        "VC設定を更新しました。"
    } else {
        "現在のVC設定です。"
    };
    format!(
        "{}システム読み上げは{}。自動読み上げは{}。読み上げモードは{}です。",
        action,
        bool_enabled_label(system_read),
        bool_enabled_label(auto_read),
        read_mode_label(parallel_count),
    )
}

pub(super) fn read_mode_label(parallel_count: usize) -> String {
    if parallel_count > 1 {
        format!("並列 {}", parallel_count)
    } else {
        "逐次".to_string()
    }
}

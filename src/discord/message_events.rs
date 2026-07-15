use std::error::Error;

use log::{debug, warn};
use serenity::all::{CreateMessage, Message};

use crate::{
    app::context::NelfieContext,
    discord::{
        images::{MessageImageAttachment, message_image_attachments},
        responses::schedule_message_response,
    },
    llm::context::{LMContext, Role},
    voice::{SpeakOptions, apply_tts_dictionaries, build_tts_text_from_message},
};

pub(super) async fn handle_emoji_reaction(
    reaction: &serenity::model::channel::Reaction,
    ob_context: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // ここでリアクション追加時の処理を実装可能
    let channel_id = reaction.channel_id;
    let message_id = reaction.message_id;
    let member = reaction.member.clone().unwrap_or_default();
    let user_id = member.user.name.clone();
    let user_display_name = member.user.display_name().to_string();
    let mut lm_context = LMContext::new();
    lm_context.add_text(
        serde_json::json!({
            "user": user_id,
            "display_name": user_display_name,
            "added_reaction": format!("{:?}", reaction.emoji),
            "message_id": message_id.to_string(),
            "channel_id": channel_id.to_string()
        })
        .to_string(),
        Role::User,
    );
    ob_context.chat_contexts.marge(channel_id, &lm_context);

    debug!(
        "Handling emoji reaction: {:?} by user {:?}",
        reaction.emoji, reaction.user_id
    );
    Ok(())
}

/// メッセージを受け取ったときの処理
pub(super) async fn handle_message(
    ctx: &serenity::client::Context,
    msg: &Message,
    _framework: poise::FrameworkContext<'_, NelfieContext, Box<dyn Error + Send + Sync>>,
    ob_context: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let channel_id = msg.channel_id;

    let bot_id = ctx.cache.current_user().id;

    let is_mentioned = msg.mentions_user_id(bot_id);

    if let Some(guild_id) = msg.guild_id
        && ob_context.chat_contexts.is_voice_auto_read(channel_id)
    {
        let user_voice =
            ob_context
                .user_contexts
                .get_or_create_actor(msg.author.id, msg.guild_id, bot_id);
        let speaker = user_voice.voice_speaker;
        let speed_scale = user_voice.voice_speed_scale;
        let pitch_scale = user_voice.voice_pitch_scale;
        let pan = user_voice.voice_pan;

        if let Some(base_text) = build_tts_text_from_message(&ctx.cache, msg) {
            let guild_dictionary = ob_context
                .chat_contexts
                .voice_dictionary_entries(channel_id);
            let user_dictionary = ob_context.user_contexts.voice_dictionary_entries_actor(
                msg.author.id,
                msg.guild_id,
                bot_id,
            );
            let text = apply_tts_dictionaries(&base_text, &guild_dictionary, &user_dictionary);
            let parallel_count = ob_context.chat_contexts.voice_parallel_count(channel_id);

            if let Err(e) = ob_context
                .voice_system
                .speak(
                    guild_id,
                    text,
                    SpeakOptions {
                        speaker,
                        speed_scale,
                        pitch_scale,
                        pan,
                        channel_id,
                        parallel_count,
                    },
                )
                .await
            {
                warn!("failed to enqueue auto-read message: {}", e);
            }
        }
    }

    let image_attachments = message_image_attachments(msg);
    let image_sources = image_attachments
        .iter()
        .filter_map(|image| image.source.clone())
        .collect::<Vec<_>>();
    let attachment_context = image_attachments
        .iter()
        .map(MessageImageAttachment::to_context_json)
        .collect::<Vec<_>>();

    let content = serde_json::json!({
        "user": msg.author.name,
        "display_name": msg.author.display_name(),
        "msg_id": msg.id.to_string(),
        "reply_to": msg.referenced_message.as_ref().map_or("None".to_string(), |m| m.id.to_string()),
        "content": msg.content,
        "attachments": attachment_context,
    }).to_string();

    let mut lm_context = LMContext::new();
    if image_sources.is_empty() {
        debug!(
            "Adding text message to context in channel {}, content: {}",
            channel_id, content
        );
        lm_context.add_text(content.clone(), Role::User);
    } else {
        debug!(
            "Adding image message to context in channel {}, content: {}",
            channel_id, content
        );
        lm_context.add_user_text_with_discord_images(content.clone(), image_sources);
    }

    ob_context.chat_contexts.marge(channel_id, &lm_context);

    if is_mentioned {
        if !ob_context.chat_contexts.is_enabled(channel_id) {
            msg.channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content("info: Chat context is disabled in this channel."),
                )
                .await?;
            return Ok(());
        }

        schedule_message_response(ctx, msg, ob_context);
    }

    Ok(())
}

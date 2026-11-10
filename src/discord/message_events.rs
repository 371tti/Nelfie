use std::error::Error;

use log::{debug, warn};
use serenity::all::{CreateMessage, Message, UserId};

use crate::{
    app::context::NelfieContext,
    discord::{
        images::{MessageImageAttachment, message_image_attachments},
        responses::{schedule_auto_replay_judge, schedule_message_response},
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
    let message_revision = ob_context.chat_contexts.record_incoming_message(channel_id);
    ob_context.attachment_registry.observe_message(msg);

    let bot_id = ctx.cache.current_user().id;

    let is_mentioned = msg.mentions_user_id(bot_id);
    record_message_context(msg, ob_context, bot_id);

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

    if msg.author.bot || msg.webhook_id.is_some() {
        if msg.author.id == bot_id
            && is_mentioned
            && ob_context.chat_contexts.is_enabled(channel_id)
            && ob_context.chat_contexts.is_auto_replay_enabled(channel_id)
        {
            schedule_auto_replay_judge(ctx, msg, ob_context, message_revision);
        }
        return Ok(());
    }

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

        ob_context
            .chat_contexts
            .invalidate_auto_replay_judge(channel_id);
        schedule_message_response(ctx, msg, ob_context);
    } else if !msg.author.bot
        && !msg.content.starts_with('!')
        && ob_context.chat_contexts.is_enabled(channel_id)
        && ob_context.chat_contexts.is_auto_replay_enabled(channel_id)
    {
        schedule_auto_replay_judge(ctx, msg, ob_context, message_revision);
    }

    Ok(())
}

fn record_message_context(msg: &Message, ob_context: &NelfieContext, bot_id: UserId) {
    let channel_id = msg.channel_id;
    let image_attachments = message_image_attachments(msg);
    let image_sources = image_attachments
        .iter()
        .filter_map(|image| image.source.clone())
        .collect::<Vec<_>>();
    let mut attachment_context = image_attachments
        .iter()
        .map(MessageImageAttachment::to_context_json)
        .collect::<Vec<_>>();
    let image_attachment_ids = image_attachments
        .iter()
        .map(|attachment| attachment.attachment_id)
        .collect::<std::collections::HashSet<_>>();
    attachment_context.extend(
        ob_context
            .attachment_registry
            .list(channel_id)
            .into_iter()
            .filter(|reference| {
                reference.message_id == Some(msg.id.get())
                    && !image_attachment_ids.contains(&reference.attachment_id)
            })
            .map(|reference| reference.to_json()),
    );

    let message_time = msg.timestamp.with_timezone(&chrono_tz::Asia::Tokyo);
    let message_minute = message_time.timestamp().div_euclid(60);
    let time_marker = format!("JST {}", message_time.format("%Y-%m-%d %H:%M:%S"));
    let mut content = format!(
        "{} (@{}) #{}",
        msg.author.display_name(),
        msg.author.name,
        msg.id
    );
    if let Some(reply) = msg.referenced_message.as_ref() {
        content.push_str(&format!(" reply:#{}", reply.id));
    }
    content.push_str(": ");
    content.push_str(&escape_message_line(&msg.content));
    if !attachment_context.is_empty() {
        content.push_str(" attachments:");
        content.push_str(&serde_json::to_string(&attachment_context).unwrap_or_default());
    }
    let attachment_count = msg.attachments.len();
    let role = if msg.author.id == bot_id {
        Role::Assistant
    } else {
        Role::User
    };

    let mut lm_context = LMContext::new();
    if image_sources.is_empty() {
        debug!("Adding text message to context in channel {}", channel_id);
        lm_context.add_text(content, role);
    } else {
        debug!(
            "Adding image message to context in channel {} ({} attachment(s))",
            channel_id, attachment_count
        );
        lm_context.add_text_with_discord_images(content, image_sources, role);
    }

    ob_context.chat_contexts.marge_discord_message(
        channel_id,
        message_minute,
        &time_marker,
        &lm_context,
    );
}

fn escape_message_line(content: &str) -> String {
    content
        .replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

use std::error::Error;

use log::{info, warn};
use serenity::all::{ChannelId, CreateMessage, GuildId, VoiceState};
use tokio::time::{Duration, MissedTickBehavior, interval, sleep};

use crate::{
    app::context::NelfieContext,
    voice::{SpeakOptions, apply_tts_dictionaries},
};

const AUTO_LEAVE_SWEEP_INTERVAL: Duration = Duration::from_secs(15);
const AUTO_LEAVE_EVENT_DELAY: Duration = Duration::from_secs(1);
const AUTO_LEAVE_RECHECK_DELAY: Duration = Duration::from_secs(1);

pub(crate) fn start_auto_leave_monitor(ctx: serenity::client::Context, ob_context: NelfieContext) {
    tokio::spawn(async move {
        let mut sweep = interval(AUTO_LEAVE_SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(MissedTickBehavior::Skip);
        sweep.tick().await;

        loop {
            sweep.tick().await;
            let checks = ob_context
                .voice_system
                .connected_voice_guild_ids()
                .into_iter()
                .map(|guild_id| {
                    let ctx = ctx.clone();
                    let ob_context = ob_context.clone();
                    tokio::spawn(async move {
                        leave_if_voice_channel_is_empty(&ctx, &ob_context, guild_id, None).await;
                    })
                })
                .collect::<Vec<_>>();
            for check in checks {
                if let Err(error) = check.await {
                    warn!("empty voice channel monitor task failed: {}", error);
                }
            }
        }
    });
}

pub(super) async fn handle_voice_state_update(
    ctx: &serenity::client::Context,
    old: Option<&VoiceState>,
    new: &VoiceState,
    ob_context: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(guild_id) = new
        .guild_id
        .or_else(|| old.and_then(|state| state.guild_id))
    else {
        return Ok(());
    };

    if new.user_id == ctx.cache.current_user().id {
        return Ok(());
    }

    if new
        .member
        .as_ref()
        .map(|member| member.user.bot)
        .unwrap_or(false)
    {
        return Ok(());
    }

    let Some(bot_voice_channel_raw) = ob_context
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await
    else {
        return Ok(());
    };
    let bot_voice_channel = ChannelId::new(bot_voice_channel_raw);

    let old_channel = old.and_then(|state| state.channel_id);
    let new_channel = new.channel_id;

    if old_channel == new_channel {
        return Ok(());
    }

    let joined_bot_channel =
        old_channel != Some(bot_voice_channel) && new_channel == Some(bot_voice_channel);
    let left_bot_channel =
        old_channel == Some(bot_voice_channel) && new_channel != Some(bot_voice_channel);

    if !joined_bot_channel && !left_bot_channel {
        return Ok(());
    }

    if left_bot_channel {
        let task_ctx = ctx.clone();
        let task_context = ob_context.clone();
        tokio::spawn(async move {
            sleep(AUTO_LEAVE_EVENT_DELAY).await;
            leave_if_voice_channel_is_empty(
                &task_ctx,
                &task_context,
                guild_id,
                Some(bot_voice_channel),
            )
            .await;
        });

        if is_bot_voice_channel_empty(ctx, guild_id, bot_voice_channel) {
            return Ok(());
        }
    }

    let Some(text_channel) = ob_context.voice_system.config(guild_id).text_channel_id else {
        return Ok(());
    };

    if !ob_context.chat_contexts.is_voice_system_read(text_channel) {
        return Ok(());
    }

    let display_name = new
        .member
        .as_ref()
        .map(|member| member.display_name().to_string())
        .or_else(|| {
            old.and_then(|state| {
                state
                    .member
                    .as_ref()
                    .map(|member| member.display_name().to_string())
            })
        })
        .unwrap_or_else(|| format!("ユーザー{}", new.user_id.get()));

    let phrase = if joined_bot_channel {
        format!("{} がボイスチャンネルに参加しました。", display_name)
    } else {
        format!("{} がボイスチャンネルから退出しました。", display_name)
    };

    let guild_dictionary = ob_context
        .chat_contexts
        .voice_dictionary_entries(text_channel);
    let user_dictionary = ob_context
        .user_contexts
        .voice_dictionary_entries(new.user_id);
    let phrase = apply_tts_dictionaries(&phrase, &guild_dictionary, &user_dictionary);
    let parallel_count = ob_context.chat_contexts.voice_parallel_count(text_channel);

    if let Err(e) = ob_context
        .voice_system
        .speak(
            guild_id,
            phrase,
            SpeakOptions {
                speaker: None,
                speed_scale: None,
                pitch_scale: None,
                pan: None,
                channel_id: text_channel,
                parallel_count,
            },
        )
        .await
    {
        warn!("failed to enqueue join/leave announcement: {}", e);
    }

    Ok(())
}

async fn leave_if_voice_channel_is_empty(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    guild_id: GuildId,
    expected_channel: Option<ChannelId>,
) {
    let Some(channel_raw) = ob_context
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await
    else {
        return;
    };
    let channel_id = ChannelId::new(channel_raw);
    if expected_channel.is_some_and(|expected| expected != channel_id)
        || !is_bot_voice_channel_empty(ctx, guild_id, channel_id)
    {
        return;
    }

    sleep(AUTO_LEAVE_RECHECK_DELAY).await;
    if ob_context
        .voice_system
        .current_voice_channel_raw(guild_id)
        .await
        != Some(channel_raw)
        || !is_bot_voice_channel_empty(ctx, guild_id, channel_id)
    {
        return;
    }

    if let Err(error) = ob_context.voice_system.leave_voice(guild_id).await {
        warn!("failed to auto-leave empty voice channel: {}", error);
        return;
    }

    if let Some(text_channel) = ob_context.voice_system.config(guild_id).text_channel_id {
        ob_context
            .chat_contexts
            .set_voice_auto_read(text_channel, false);
        ob_context
            .voice_system
            .set_auto_read(guild_id, false, Some(text_channel));

        if let Err(error) = text_channel
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content("ボイスチャンネルに誰もいなくなったため、自動でVCから切断しました。"),
            )
            .await
        {
            warn!(
                "failed to send auto-leave message to text channel: {}",
                error
            );
        }
    }

    info!(
        "Auto-left guild {} voice channel {} because no other users remained",
        guild_id.get(),
        channel_id.get()
    );
}

fn is_bot_voice_channel_empty(
    ctx: &serenity::client::Context,
    guild_id: GuildId,
    bot_voice_channel: ChannelId,
) -> bool {
    let bot_user_id = ctx.cache.current_user().id;
    let Some(guild) = ctx.cache.guild(guild_id) else {
        return false;
    };

    !guild.voice_states.iter().any(|(user_id, state)| {
        if *user_id == bot_user_id {
            return false;
        }

        state.channel_id == Some(bot_voice_channel)
    })
}

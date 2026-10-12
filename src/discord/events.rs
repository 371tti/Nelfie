use std::error::Error;

use log::{debug, info};
use serenity::all::{ActivityData, FullEvent};
use serenity::gateway::ConnectionStage;

use crate::{
    app::context::NelfieContext,
    discord::{
        interactions::handle_interaction,
        message_events::{handle_emoji_reaction, handle_message},
        voice_events::handle_voice_state_update,
    },
};

/// イベントハンドラ
/// serenity poise へ渡すもの
pub async fn event_handler(
    ctx: &serenity::client::Context,
    event: &FullEvent,
    framework: poise::FrameworkContext<'_, NelfieContext, Box<dyn Error + Send + Sync>>,
    data: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match event {
        // メッセージうけとったとき
        FullEvent::Message { new_message } => {
            handle_message(ctx, new_message, framework, data).await?;
        }
        // 初期化完了
        FullEvent::Ready { data_about_bot } => {
            data.monitoring.mark_gateway_ready();
            info!("Bot is connected as {}", data_about_bot.user.name);
            update_presence(ctx).await;
        }
        FullEvent::Resume { .. } => {
            data.monitoring.mark_gateway_ready();
            info!("Discord gateway session resumed");
        }
        FullEvent::ShardStageUpdate { event } => match event.new {
            ConnectionStage::Disconnected => data.monitoring.mark_gateway_disconnected(),
            stage if stage.is_connecting() => data.monitoring.mark_gateway_reconnecting(),
            _ => {}
        },
        // あたらしいギルドに参加
        FullEvent::GuildCreate { guild, is_new } => {
            if is_new.unwrap_or(false) {
                info!(
                    "Joined Discord guild: name={:?} id={}",
                    guild.name, guild.id
                );
            } else {
                debug!(
                    "Discord guild available: name={:?} id={}",
                    guild.name, guild.id
                );
            }
            update_presence(ctx).await;
        }
        FullEvent::ChannelDelete {
            channel,
            messages: _,
        } => {
            remove_channel_context(channel.id, data);
        }
        FullEvent::ThreadDelete {
            thread,
            full_thread_data: _,
        } => {
            remove_channel_context(thread.id, data);
        }
        // リアクション通知
        FullEvent::ReactionAdd { add_reaction } => {
            debug!(
                "Reaction added: {:?} by user {:?}",
                add_reaction.emoji, add_reaction.user_id
            );
            handle_emoji_reaction(add_reaction, data).await?;
        }
        FullEvent::InteractionCreate { interaction } => {
            handle_interaction(ctx, interaction, data).await?;
        }
        FullEvent::VoiceStateUpdate { old, new } => {
            handle_voice_state_update(ctx, old.as_ref(), new, data).await?;
        }

        _ => { /* 他のイベントは無視 */ }
    }

    Ok(())
}

/// ステータスメッセージの更新
async fn update_presence(ctx: &serenity::client::Context) {
    let guild_count = ctx.cache.guilds().len();

    ctx.set_activity(Some(ActivityData::playing(format!(
        "in {} servers",
        guild_count
    ))));
}

fn remove_channel_context(channel_id: serenity::all::ChannelId, ob_context: &NelfieContext) {
    if ob_context.chat_contexts.remove_channel(channel_id) {
        info!(
            "Removed chat context for deleted channel {}",
            channel_id.get()
        );
    }
}

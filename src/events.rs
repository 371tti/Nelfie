use std::{
    error::Error,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use log::{debug, info, warn};
use serenity::all::{
    ActionRowComponent, ActivityData, ChannelId, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditMessage, FullEvent, GuildId, Interaction,
    Message, VoiceState,
};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    commands::log_err,
    context::NelfieContext,
    lmclient::{LMContext, Role},
    voice::{SpeakOptions, apply_tts_dictionary, build_tts_text_from_message},
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
            info!("Bot is connected as {}", data_about_bot.user.name);
            update_presence(ctx).await;
        }
        // あたらしいギルドに参加
        FullEvent::GuildCreate { guild, is_new: _ } => {
            info!("Joined new guild: {} (id: {})", guild.name, guild.id);
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

async fn handle_voice_state_update(
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

    if left_bot_channel && is_bot_voice_channel_empty(ctx, guild_id, bot_voice_channel) {
        if let Err(e) = ob_context.voice_system.leave_voice(guild_id).await {
            warn!("failed to auto-leave empty voice channel: {}", e);
            return Ok(());
        }

        if let Some(text_channel) = ob_context.voice_system.config(guild_id).text_channel_id {
            ob_context
                .chat_contexts
                .set_voice_auto_read(text_channel, false);
            ob_context
                .voice_system
                .set_auto_read(guild_id, false, Some(text_channel));

            if let Err(e) = text_channel
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(
                        "ボイスチャンネルに誰もいなくなったため、自動でVCから切断しました。",
                    ),
                )
                .await
            {
                warn!("failed to send auto-leave message to text channel: {}", e);
            }
        }

        info!(
            "Auto-left guild {} voice channel {} because no other users remained",
            guild_id.get(),
            bot_voice_channel.get()
        );
        return Ok(());
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

    let dictionary = ob_context
        .chat_contexts
        .voice_dictionary_entries(text_channel);
    let phrase = apply_tts_dictionary(&phrase, &dictionary);
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

async fn handle_interaction(
    ctx: &serenity::client::Context,
    interaction: &Interaction,
    ob_context: &NelfieContext,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match interaction {
        Interaction::Component(component) => {
            let trigger_id = component.data.custom_id.clone();
            let Some((_key, pending)) = ob_context.pending_modals.remove(&trigger_id) else {
                return Ok(());
            };

            let modal = crate::tools::modal_builder::build_create_modal(
                &pending.modal,
                &pending.submit_custom_id,
            );

            component
                .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                .await?;
        }
        Interaction::Modal(modal) => {
            if !modal
                .data
                .custom_id
                .starts_with(crate::tools::modal_builder::MODAL_SUBMIT_PREFIX)
            {
                return Ok(());
            }

            let mut fields = serde_json::Map::new();
            for row in &modal.data.components {
                for component in &row.components {
                    if let ActionRowComponent::InputText(input) = component {
                        fields.insert(
                            input.custom_id.clone(),
                            serde_json::Value::String(input.value.clone().unwrap_or_default()),
                        );
                    }
                }
            }

            modal
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("モーダル送信を受け付けたよ。")
                            .ephemeral(true),
                    ),
                )
                .await?;

            let mut lm_context = LMContext::new();
            lm_context.add_text(
                serde_json::json!({
                    "user": modal.user.name,
                    "display_name": modal.user.display_name(),
                    "type": "modal_submit",
                    "modal_custom_id": modal.data.custom_id,
                    "channel_id": modal.channel_id.to_string(),
                    "fields": fields,
                })
                .to_string(),
                Role::User,
            );
            ob_context
                .chat_contexts
                .marge(modal.channel_id, &lm_context);
        }
        _ => {}
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

async fn handle_emoji_reaction(
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
async fn handle_message(
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
        let user_voice = ob_context.user_contexts.get_or_create(msg.author.id);
        let speaker = user_voice.voice_speaker;
        let speed_scale = user_voice.voice_speed_scale;
        let pitch_scale = user_voice.voice_pitch_scale;
        let pan = user_voice.voice_pan;

        if let Some(base_text) = build_tts_text_from_message(&ctx.cache, msg) {
            let dictionary = ob_context
                .chat_contexts
                .voice_dictionary_entries(channel_id);
            let text = apply_tts_dictionary(&base_text, &dictionary);
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

    let content = serde_json::json!({
        "user": msg.author.name,
        "display_name": msg.author.display_name(),
        "msg_id": msg.id.to_string(),
        "reply_to": msg.referenced_message.as_ref().map_or("None".to_string(), |m| m.id.to_string()),
        "content": msg.content
    }).to_string();

    // 添付画像のURLを取る
    let image_urls: Vec<String> = msg
        .attachments
        .iter()
        .filter(|att| {
            // content_type が "image/..." なら画像とみなす
            if let Some(ct) = &att.content_type {
                ct.starts_with("image/")
            } else {
                // 拡張子で雑に判定する fallback
                att.filename.ends_with(".png")
                    || att.filename.ends_with(".jpg")
                    || att.filename.ends_with(".jpeg")
                    || att.filename.ends_with(".webp")
            }
        })
        .map(|att| att.url.clone())
        .collect();

    let mut lm_context = LMContext::new();

    if image_urls.is_empty() && content.is_empty() {
        // 画像もテキストも無いなら無視
        return Ok(());
    } else if image_urls.is_empty() {
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
        lm_context.add_user_text_with_images(content.clone(), image_urls.clone());
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

        schedule_latest_response(ctx, msg, ob_context);
    }

    Ok(())
}

fn schedule_latest_response(
    ctx: &serenity::client::Context,
    msg: &Message,
    ob_context: &NelfieContext,
) {
    let channel_id = msg.channel_id;
    let request_id = ob_context.response_seq.fetch_add(1, Ordering::Relaxed);

    ob_context.responding_channels.insert(channel_id, true);

    let ctx_cloned = ctx.clone();
    let msg_cloned = msg.clone();
    let ob_ctx_cloned = ob_context.clone();

    let task_handle = tokio::spawn(async move {
        if let Err(e) =
            run_response_task(&ctx_cloned, &msg_cloned, &ob_ctx_cloned, request_id).await
        {
            log_err("run_response_task", e.as_ref());
        }

        if let Some(current) = ob_ctx_cloned.active_responses.get(&channel_id)
            && current.request_id == request_id
        {
            drop(current);
            ob_ctx_cloned.active_responses.remove(&channel_id);
            ob_ctx_cloned.responding_channels.insert(channel_id, false);
        }
    });

    let new_active = crate::context::ActiveResponse {
        request_id,
        abort_handle: task_handle.abort_handle(),
    };

    if let Some(old_active) = ob_context.active_responses.insert(channel_id, new_active) {
        old_active.abort_handle.abort();
    }
}

async fn run_response_task(
    ctx: &serenity::client::Context,
    msg: &Message,
    ob_context: &NelfieContext,
    request_id: u64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();
    let channel_id = msg.channel_id;
    let user_id = msg.author.id;

    let user_ctx = ob_context.user_contexts.get_or_create(user_id);
    let model = user_ctx.main_model.clone();

    let model_cost = model.rate_cost();
    let sec_per_cost = ob_context.config.rate_limit_sec_per_cost;
    let window_size = ob_context.config.rale_limit_window_size;
    let user_line = user_ctx.rate_line;

    let time_stamp = chrono::Utc::now().timestamp() as u64;
    let limit_line = window_size + time_stamp;
    let add_line = model_cost * sec_per_cost;
    let added_user_line = if user_line == 0 {
        0
    } else if user_line < time_stamp {
        time_stamp + add_line
    } else {
        user_line + add_line
    };

    if added_user_line > limit_line {
        let wait_sec = added_user_line - limit_line;
        let allow_ts = time_stamp + wait_sec;
        msg.channel_id
            .send_message(
                &ctx.http,
                CreateMessage::new().content(format!(
                    "Err: rate limit - try again after <t:{}:R>",
                    allow_ts
                )),
            )
            .await?;
        return Ok(());
    }
    ob_context
        .user_contexts
        .set_rate_line(user_id, added_user_line);

    let typing_ctx = ctx.clone();
    let typing_ob_ctx = ob_context.clone();
    let typing_channel_id = channel_id;
    let typing_handle = tokio::spawn(async move {
        loop {
            let still_current = typing_ob_ctx
                .active_responses
                .get(&typing_channel_id)
                .map(|active| active.request_id == request_id)
                .unwrap_or(false);

            if !still_current {
                break;
            }

            let _ = typing_channel_id.broadcast_typing(&typing_ctx.http).await;
            sleep(Duration::from_secs(5)).await;
        }
    });

    let mut context = ob_context.chat_contexts.get_or_create(channel_id);
    let tools = ob_context.tools.clone();

    let system_prompt = format!(
        "{}\n current guild_id: {}, current channel_id: {}, channel_name: {}",
        ob_context.chat_contexts.get_system_prompt(channel_id),
        msg.guild_id
            .map(|id| id.get().to_string())
            .unwrap_or_else(|| "None".to_string()),
        msg.channel_id,
        msg.channel_id
            .name(&ctx.http)
            .await
            .unwrap_or("None".to_string()),
    );

    context.add_text(system_prompt, Role::System);

    let mut thinking_msg = msg
        .channel_id
        .send_message(&ctx.http, CreateMessage::new().content("-# Thinking..."))
        .await?;

    let (state_tx, mut state_rx) = mpsc::channel::<String>(100);
    let (delta_tx, mut delta_rx) = mpsc::channel::<String>(100);

    let state_http = ctx.http.clone();
    let state_msg_id = thinking_msg.id;
    let state_channel = thinking_msg.channel_id;
    let state_reader = tokio::spawn(async move {
        let mut last_edit = Instant::now() + Duration::from_millis(550);

        while let Some(state) = state_rx.recv().await {
            if last_edit.elapsed() < Duration::from_millis(550) {
                continue;
            }

            let _ = state_channel
                .edit_message(
                    &state_http,
                    state_msg_id,
                    EditMessage::new().content(format!("-# {}", state)),
                )
                .await;
            last_edit = Instant::now();
        }
    });

    let delta_reader = tokio::spawn(async move {
        while let Some(delta) = delta_rx.recv().await {
            info!("Delta received: {}", delta);
        }
    });

    let timeout_duration = Duration::from_millis(ob_context.config.timeout_millis);
    let result = match tokio::time::timeout(
        timeout_duration,
        ob_context.lm_client.generate_response(
            ob_context.clone(),
            &context,
            Some(2000),
            Some(tools),
            Some(state_tx),
            Some(delta_tx),
            Some(model.to_parameter()),
        ),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            log_err("Error generating response", e.as_ref());
            thinking_msg
                .edit(
                    &ctx.http,
                    EditMessage::new().content("-# Error during reasoning"),
                )
                .await
                .ok();
            typing_handle.abort();
            state_reader.abort();
            delta_reader.abort();
            return Err(e);
        }
        Err(_) => {
            thinking_msg
                .edit(&ctx.http, EditMessage::new().content("-# Error timeout"))
                .await
                .ok();
            typing_handle.abort();
            state_reader.abort();
            delta_reader.abort();
            return Ok(());
        }
    };

    state_reader.abort();
    delta_reader.abort();

    let still_current = ob_context
        .active_responses
        .get(&channel_id)
        .map(|active| active.request_id == request_id)
        .unwrap_or(false);
    if !still_current {
        typing_handle.abort();
        return Ok(());
    }

    ob_context.chat_contexts.marge(channel_id, &result);

    let elapsed = start.elapsed().as_millis();
    let text = result.get_result();
    let sent_via_discord_tool = result.get_latest_discord_send_content();

    debug!("Final response: {}ms \"{}\"", elapsed, text);

    typing_handle.abort();

    let model = ob_context.user_contexts.get_or_create(user_id).main_model;

    if let Err(e) = thinking_msg.delete(&ctx.http).await {
        warn!("failed to delete thinking message: {}", e);
    }

    if let Some(sent_text) = sent_via_discord_tool
        && is_same_text_loosely(&sent_text, &text)
    {
        info!("Skipping final assistant send because discord-tool already posted the same text");
        return Ok(());
    }

    msg.channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new().content(format!(
                "{}\n-# Reasoning done in {}ms, model: {}",
                text, elapsed, model
            )),
        )
        .await?;

    Ok(())
}

fn is_same_text_loosely(a: &str, b: &str) -> bool {
    normalize_text(a) == normalize_text(b)
}

fn normalize_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

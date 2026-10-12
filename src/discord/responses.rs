use std::{
    error::Error,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use log::{debug, info, warn};
use serenity::all::{
    CreateMessage, EditInteractionResponse, EditMessage, Message, ModalInteraction,
};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    app::context::NelfieContext,
    discord::{
        ephemeral::scope_ephemeral_interaction,
        images::refresh_discord_image_urls_for_api,
        logging::log_err,
        message_delivery::{
            MODAL_PUBLIC_RESPONSE_MESSAGE, send_generated_images, send_long_message,
            send_modal_ephemeral_response, send_modal_generated_images,
        },
    },
    llm::{
        client::GenerateResponseOptions, compaction::schedule_context_compaction, context::Role,
        user::UserContextScope,
    },
};

mod origin;
mod rate_limit;

pub use origin::CronResponseRequest;
use origin::{RateLimitBucket, RateLimitTarget, ResponseOrigin, UserResponseRequest};
use rate_limit::{add_rate_usage, advance_rate_limit_bucket};

const IMAGE_GENERATION_WORKFLOW_INSTRUCTIONS: &str = concat!(
    "\nImage generation workflow: Before generating or editing an image, call `model-control` ",
    "with target `image_generation` to read or set the selected model. The built-in ",
    "image_generation tool is enabled on the next API round using that selection. Do not ",
    "request image generation in the same round as changing the image model."
);

pub(super) fn schedule_message_response(
    ctx: &serenity::client::Context,
    msg: &Message,
    ob_context: &NelfieContext,
) {
    schedule_response(
        ctx,
        ob_context,
        ResponseOrigin::User(UserResponseRequest::from_message(msg)),
    );
}

pub(super) fn schedule_modal_response(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    modal: &ModalInteraction,
) {
    schedule_response(
        ctx,
        ob_context,
        ResponseOrigin::User(UserResponseRequest::from_modal(modal)),
    );
}

pub fn schedule_cron_response(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    cron: CronResponseRequest,
) {
    schedule_response(ctx, ob_context, ResponseOrigin::Cron(cron));
}

fn schedule_response(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    origin: ResponseOrigin,
) {
    let channel_id = origin.channel_id();
    let request_id = ob_context.response_seq.fetch_add(1, Ordering::Relaxed);

    ob_context.responding_channels.insert(channel_id, true);

    let ctx_cloned = ctx.clone();
    let ob_ctx_cloned = ob_context.clone();
    let origin_cloned = origin.clone();

    let task_handle = tokio::spawn(async move {
        if let Err(e) =
            run_response_task(&ctx_cloned, &ob_ctx_cloned, origin_cloned, request_id).await
        {
            log_err(
                &format!(
                    "response failed: request_id={} channel={}",
                    request_id,
                    channel_id.get()
                ),
                e.as_ref(),
            );
        }

        if let Some(current) = ob_ctx_cloned.active_responses.get(&channel_id)
            && current.request_id == request_id
        {
            drop(current);
            ob_ctx_cloned.active_responses.remove(&channel_id);
            ob_ctx_cloned.responding_channels.insert(channel_id, false);
        }
    });

    let new_active = crate::app::context::ActiveResponse {
        request_id,
        abort_handle: task_handle.abort_handle(),
    };

    if let Some(old_active) = ob_context.active_responses.insert(channel_id, new_active) {
        old_active.abort_handle.abort();
    }
}

async fn run_response_task(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    origin: ResponseOrigin,
    request_id: u64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();
    let channel_id = origin.channel_id();
    let guild_id = origin.guild_id();
    let bot_user_id = ctx.cache.current_user().id;
    let rate_limit_target = origin.rate_limit_target(bot_user_id);
    let rate_limit_bucket = origin.rate_limit_bucket();
    let user_context_scope = match rate_limit_target {
        RateLimitTarget::User(user_id) => UserContextScope::User(user_id),
        RateLimitTarget::GuildBot {
            guild_id,
            bot_user_id,
        } => UserContextScope::GuildBot {
            guild_id,
            bot_user_id,
        },
    };

    let user_ctx = ob_context
        .user_contexts
        .get_or_create_scoped(user_context_scope);
    let model = origin.response_model(user_ctx.main_model);
    let source = origin.log_source();
    let guild = guild_id
        .map(|id| id.get().to_string())
        .unwrap_or_else(|| "dm".to_string());
    let (current_rate_line, add_line, window_size, rate_limit_label) = match rate_limit_bucket {
        RateLimitBucket::General => (
            user_ctx.rate_line,
            ob_context
                .config
                .rate_limit_sec_per_cost
                .saturating_mul(model.rate_cost()),
            ob_context.config.rate_limit_window_size,
            "rate limit",
        ),
        RateLimitBucket::Cron => (
            user_ctx.cron_rate_line,
            ob_context.config.cron_rate_limit_sec_per_run,
            ob_context.config.cron_rate_limit_window_size,
            "cron rate limit",
        ),
    };

    let time_stamp = chrono::Utc::now().timestamp() as u64;
    let added_rate_line = match advance_rate_limit_bucket(
        current_rate_line,
        time_stamp,
        add_line,
        window_size,
    ) {
        Ok(rate_line) => rate_line,
        Err(allow_ts) => {
            let reason = format!("{rate_limit_label} - try again after <t:{allow_ts}:R>");
            if let ResponseOrigin::Cron(cron) = &origin {
                warn!(
                    "response skipped: request_id={} source={} channel={} cron_id={} reason=rate_limit retry_at={}",
                    request_id,
                    source,
                    channel_id.get(),
                    cron.cron_id,
                    allow_ts,
                );
            } else {
                info!(
                    "response skipped: request_id={} source={} channel={} reason=rate_limit retry_at={}",
                    request_id,
                    source,
                    channel_id.get(),
                    allow_ts,
                );
            }
            let message = origin
                .failure_message(&reason)
                .unwrap_or_else(|| format!("Err: {reason}"));
            channel_id
                .send_message(&ctx.http, CreateMessage::new().content(message))
                .await?;
            return Ok(());
        }
    };

    match rate_limit_bucket {
        RateLimitBucket::General => ob_context
            .user_contexts
            .set_rate_line_scoped(user_context_scope, added_rate_line),
        RateLimitBucket::Cron => ob_context
            .user_contexts
            .set_cron_rate_line_scoped(user_context_scope, added_rate_line),
    }

    info!(
        "response started: request_id={} source={} channel={} guild={} model={}",
        request_id,
        source,
        channel_id.get(),
        guild,
        model
    );

    let typing_handle = if !origin.uses_public_progress() {
        None
    } else {
        let typing_ctx = ctx.clone();
        let typing_ob_ctx = ob_context.clone();
        let typing_channel_id = channel_id;
        Some(tokio::spawn(async move {
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
        }))
    };

    let request_context = origin.current_request_context();
    let mut context = ob_context.chat_contexts.get_or_create(channel_id);
    if let Some(request_context) = request_context.as_ref() {
        context.extend(request_context);
    }

    let tools = ob_context.tools.clone();
    let channel_name = channel_id
        .name(&ctx.http)
        .await
        .unwrap_or("None".to_string());

    let mut system_prompt = format!(
        "{}\n current guild_id: {}, current channel_id: {}, channel_name: {}",
        ob_context.chat_contexts.get_system_prompt(channel_id),
        guild_id
            .map(|id| id.get().to_string())
            .unwrap_or_else(|| "None".to_string()),
        channel_id,
        channel_name,
    );
    if let Some(note) = origin.system_note() {
        system_prompt.push('\n');
        system_prompt.push_str(&note);
    }
    system_prompt.push_str(IMAGE_GENERATION_WORKFLOW_INSTRUCTIONS);

    refresh_discord_image_urls_for_api(ctx, &mut context).await;

    context.add_text(system_prompt, Role::System);

    let mut thinking_msg = if !origin.uses_public_progress() {
        None
    } else {
        Some(
            channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(origin.thinking_label()),
                )
                .await?,
        )
    };

    let (state_tx, state_rx) = mpsc::channel::<String>(100);
    let (rate_usage_tx, mut rate_usage_rx) = mpsc::unbounded_channel::<u64>();
    let rate_usage_context = ob_context.clone();
    let rate_usage_task = tokio::spawn(async move {
        while let Some(cost_units) = rate_usage_rx.recv().await {
            if cost_units != 0 {
                charge_additional_rate_usage(
                    &rate_usage_context,
                    user_context_scope,
                    rate_limit_bucket,
                    cost_units,
                );
            }
        }
    });

    let state_reader = thinking_msg.as_ref().map(|thinking_msg| {
        let state_http = ctx.http.clone();
        let state_msg_id = thinking_msg.id;
        let state_channel = thinking_msg.channel_id;
        let mut state_rx = state_rx;
        tokio::spawn(async move {
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
        })
    });

    let timeout_duration = Duration::from_millis(ob_context.config.timeout_millis);
    let context_cache_key = format!("nelfie-context-{}", channel_id.get());
    let generate_response = ob_context.lm_client.generate_response(
        ob_context.clone(),
        &context,
        GenerateResponseOptions {
            max_output_tokens: 2_000,
            tools,
            state_sender: Some(state_tx),
            context_cache_key,
            parameters: model.to_parameter(),
            user_context_scope,
            allow_mainline_model_change: matches!(&origin, ResponseOrigin::User(_)),
            rate_usage_sender: rate_usage_tx,
        },
    );
    let result = match tokio::time::timeout(timeout_duration, async {
        if let Some(ephemeral_context) = origin.ephemeral_interaction_context() {
            scope_ephemeral_interaction(ephemeral_context, generate_response).await
        } else {
            generate_response.await
        }
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            let message = origin
                .failure_message("reasoning error")
                .unwrap_or_else(|| "-# Error during reasoning".to_string());
            if let Some(modal) = origin.modal_ephemeral_delivery() {
                modal
                    .interaction
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                    .await
                    .ok();
            } else if let Some(thinking_msg) = thinking_msg.as_mut() {
                thinking_msg
                    .edit(&ctx.http, EditMessage::new().content(message))
                    .await
                    .ok();
            }
            if let Some(typing_handle) = typing_handle.as_ref() {
                typing_handle.abort();
            }
            if let Some(state_reader) = state_reader.as_ref() {
                state_reader.abort();
            }
            let _ = rate_usage_task.await;
            return Err(e);
        }
        Err(_) => {
            let message = origin
                .failure_message("timeout")
                .unwrap_or_else(|| "-# Error timeout".to_string());
            if let Some(modal) = origin.modal_ephemeral_delivery() {
                modal
                    .interaction
                    .edit_response(&ctx.http, EditInteractionResponse::new().content(message))
                    .await
                    .ok();
            } else if let Some(thinking_msg) = thinking_msg.as_mut() {
                thinking_msg
                    .edit(&ctx.http, EditMessage::new().content(message))
                    .await
                    .ok();
            }
            if let Some(typing_handle) = typing_handle.as_ref() {
                typing_handle.abort();
            }
            if let Some(state_reader) = state_reader.as_ref() {
                state_reader.abort();
            }
            let _ = rate_usage_task.await;
            warn!(
                "response timed out: request_id={} source={} channel={} timeout_ms={}",
                request_id,
                source,
                channel_id.get(),
                ob_context.config.timeout_millis
            );
            return Ok(());
        }
    };

    let _ = rate_usage_task.await;

    if let Some(state_reader) = state_reader.as_ref() {
        state_reader.abort();
    }
    let still_current = ob_context
        .active_responses
        .get(&channel_id)
        .map(|active| active.request_id == request_id)
        .unwrap_or(false);
    if !still_current {
        if let Some(typing_handle) = typing_handle.as_ref() {
            typing_handle.abort();
        }
        return Ok(());
    }

    let generated_images = result.generated_images;
    let final_model = result.final_model;
    let result = result.context;

    ob_context.chat_contexts.marge(channel_id, &result);
    if origin.triggers_context_compaction() {
        schedule_context_compaction(ob_context, channel_id);
    }

    let elapsed = start.elapsed().as_millis();
    let text = result.get_result();
    let sent_via_discord_tool = result.get_latest_discord_send_record();
    let total_tokens = result
        .response_total_tokens()
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let (cache_status, cached_tokens, input_tokens) = result
        .response_cache_usage()
        .map(|usage| {
            (
                if usage.hit() { "hit" } else { "miss" },
                usage.cached_tokens.to_string(),
                usage.input_tokens.to_string(),
            )
        })
        .unwrap_or_else(|| ("unknown", "unknown".to_string(), "unknown".to_string()));

    info!(
        "response generated: request_id={} source={} channel={} model={} elapsed_ms={} total_tokens={} prompt_cache={} cached_tokens={}/{} output_chars={}",
        request_id,
        source,
        channel_id.get(),
        final_model,
        elapsed,
        total_tokens,
        cache_status,
        cached_tokens,
        input_tokens,
        text.chars().count()
    );
    info!("response output: request_id={} text={}", request_id, text);

    if let Some(typing_handle) = typing_handle.as_ref() {
        typing_handle.abort();
    }

    if let Some(thinking_msg) = thinking_msg.as_ref()
        && let Err(e) = thinking_msg.delete(&ctx.http).await
    {
        warn!("failed to delete thinking message: {}", e);
    }

    let footer = format!("-# Reasoning done in {}ms, model: {}", elapsed, final_model);

    if let Some(modal) = origin.modal_ephemeral_delivery() {
        if let Some(sent) = sent_via_discord_tool {
            if sent.private {
                debug!(
                    "Skipping final assistant send because discord-tool already sent an ephemeral interaction response"
                );
                send_modal_generated_images(&ctx.http, &modal.interaction, generated_images)
                    .await?;
                return Ok(());
            }

            debug!(
                "Skipping final assistant send because discord-tool already posted a public modal response"
            );
            modal
                .interaction
                .edit_response(
                    &ctx.http,
                    EditInteractionResponse::new().content(MODAL_PUBLIC_RESPONSE_MESSAGE),
                )
                .await
                .ok();
            send_generated_images(&ctx.http, channel_id, generated_images).await?;
            return Ok(());
        }

        send_modal_ephemeral_response(&ctx.http, &modal.interaction, &text, &footer).await?;
        send_modal_generated_images(&ctx.http, &modal.interaction, generated_images).await?;
        return Ok(());
    }

    if let Some(sent) = sent_via_discord_tool {
        if sent.private {
            debug!(
                "Skipping final assistant send because discord-tool already sent an ephemeral interaction response"
            );
            if !generated_images.is_empty() {
                warn!(
                    "skipping generated image delivery because this response has no ephemeral interaction target"
                );
            }
            return Ok(());
        }

        if is_same_text_loosely(&sent.content, &text) {
            debug!(
                "Skipping final assistant send because discord-tool already posted the same text"
            );
            send_generated_images(&ctx.http, channel_id, generated_images).await?;
            return Ok(());
        }
    }

    send_long_message(&ctx.http, channel_id, &text, &footer).await?;
    send_generated_images(&ctx.http, channel_id, generated_images).await?;

    Ok(())
}

fn is_same_text_loosely(a: &str, b: &str) -> bool {
    normalize_text(a) == normalize_text(b)
}

fn charge_additional_rate_usage(
    ob_context: &NelfieContext,
    scope: UserContextScope,
    bucket: RateLimitBucket,
    cost_units: u64,
) {
    let user_context = ob_context.user_contexts.get_or_create_scoped(scope);
    let (current_line, seconds_per_unit) = match bucket {
        RateLimitBucket::General => (
            user_context.rate_line,
            ob_context.config.rate_limit_sec_per_cost,
        ),
        RateLimitBucket::Cron => (
            user_context.cron_rate_line,
            ob_context.config.cron_rate_limit_sec_per_run,
        ),
    };
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let new_line = add_rate_usage(
        current_line,
        now,
        seconds_per_unit.saturating_mul(cost_units),
    );

    match bucket {
        RateLimitBucket::General => ob_context
            .user_contexts
            .set_rate_line_scoped(scope, new_line),
        RateLimitBucket::Cron => ob_context
            .user_contexts
            .set_cron_rate_line_scoped(scope, new_line),
    }
}

fn normalize_text(s: &str) -> String {
    s.split_whitespace().collect::<Vec<&str>>().join(" ")
}

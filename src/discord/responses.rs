use std::{
    error::Error,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use log::{debug, info, warn};
use serenity::all::{
    CreateMessage, EditInteractionResponse, EditMessage, Message, ModalInteraction,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant as TokioInstant, MissedTickBehavior, interval, timeout, timeout_at},
};

use crate::{
    app::context::NelfieContext,
    discord::{
        ephemeral::scope_ephemeral_interaction,
        images::refresh_discord_image_urls_for_api,
        logging::log_err,
        message_delivery::{
            MODAL_PUBLIC_RESPONSE_MESSAGE, append_footer_to_sent_message, send_generated_images,
            send_long_message, send_modal_ephemeral_response, send_modal_generated_images,
        },
    },
    llm::{
        client::{GenerateResponseOptions, ResponseProgress},
        compaction::schedule_context_compaction,
        context::{DiscordSendRecord, Role, compaction_target_tokens},
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
    "request image generation in the same round as changing the image model.\n",
    "Attachment workflow: Use the `file` tool to list attachments in the current or another ",
    "bot-visible channel, read supported UTF-8 text files, or attach Responses API-supported ",
    "files to the next request. `get_user_profile` also registers its images as reusable file ",
    "IDs in the current channel; use those IDs from `profile_images` as Typst assets. The file ",
    "list contains IDs that remain available after history compaction. For a user-uploaded ",
    "`.typ` file, pass its attachment ID directly as `source_attachment_id`; do not read or ",
    "copy its contents into a tool argument.\n",
    "Typst workflow: `typst-render` creates a draft and returns PNG page previews without ",
    "posting. Each render has a distinct `draft_id`; inspect the matching ID on every page and ",
    "any renderer warnings in the next request. If corrections are needed, render another draft. ",
    "Call `typst-publish` with the exact `draft_id` you chose, only after that draft's preview ",
    "was reviewed in an earlier response round and only when the user requested delivery. The ",
    "default final format is PNG; use SVG or PDF only when explicitly requested. Never say the ",
    "file was posted until `typst-publish` succeeds."
);

const RESPONSE_STATUS_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_RESPONSE_OUTPUT_TOKENS: u32 = 32_000;

enum ProgressTarget {
    Public {
        channel_id: serenity::all::ChannelId,
        message_id: Option<serenity::all::MessageId>,
    },
    Modal(ModalInteraction),
}

impl ProgressTarget {
    fn is_public(&self) -> bool {
        matches!(self, Self::Public { .. })
    }

    async fn show(
        &mut self,
        http: &serenity::http::Http,
        content: String,
    ) -> Result<(), serenity::Error> {
        match self {
            Self::Public {
                channel_id,
                message_id,
            } => {
                if let Some(message_id) = message_id {
                    channel_id
                        .edit_message(http, *message_id, EditMessage::new().content(content))
                        .await?;
                } else {
                    let message = channel_id
                        .send_message(http, serenity::all::CreateMessage::new().content(content))
                        .await?;
                    *message_id = Some(message.id);
                }
            }
            Self::Modal(interaction) => {
                interaction
                    .edit_response(http, EditInteractionResponse::new().content(content))
                    .await?;
            }
        }
        Ok(())
    }

    async fn delete(&mut self, http: &serenity::http::Http) -> Result<(), serenity::Error> {
        match self {
            Self::Public {
                channel_id,
                message_id,
            } => {
                if let Some(id) = *message_id {
                    channel_id.delete_message(http, id).await?;
                    *message_id = None;
                }
            }
            Self::Modal(interaction) => interaction.delete_response(http).await?,
        }
        Ok(())
    }
}

fn render_progress(progress: &ResponseProgress) -> Option<String> {
    let content = match progress {
        ResponseProgress::Preparing => return None,
        ResponseProgress::ToolRunning { tools } => tools
            .iter()
            .map(|tool| {
                format!(
                    "-# {}: {}",
                    strip_status_prefix(&tool.label),
                    strip_status_prefix(&tool.explanation)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ResponseProgress::Failed(content) => content.clone(),
        ResponseProgress::Completed | ResponseProgress::Canceled => return None,
    };

    Some(truncate_progress_content(&content, 1900))
}

fn strip_status_prefix(value: &str) -> &str {
    let mut value = value.trim();
    while let Some(rest) = value.strip_prefix("-#") {
        value = rest.trim_start();
    }
    value
}

fn truncate_progress_content(content: &str, max_utf16_units: usize) -> String {
    let total_units = content.encode_utf16().count();
    if total_units <= max_utf16_units {
        return content.to_string();
    }
    if max_utf16_units == 0 {
        return String::new();
    }

    let content_limit = max_utf16_units - 1;
    let mut units = 0usize;
    let mut end = content.len();
    for (index, character) in content.char_indices() {
        let next_units = units + character.len_utf16();
        if next_units > content_limit {
            end = index;
            break;
        }
        units = next_units;
    }
    format!("{}…", &content[..end])
}

async fn edit_progress_target(
    http: &serenity::http::Http,
    target: &mut ProgressTarget,
    content: String,
) {
    match timeout(RESPONSE_STATUS_TIMEOUT, target.show(http, content)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!("failed to edit response progress: {error}");
        }
        Err(_) => {
            warn!("timed out editing response progress");
        }
    }
}

async fn delete_progress_target(http: &serenity::http::Http, target: &mut ProgressTarget) {
    match timeout(RESPONSE_STATUS_TIMEOUT, target.delete(http)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warn!("failed to delete response progress: {error}"),
        Err(_) => warn!("timed out deleting response progress"),
    }
}

async fn run_progress_controller(
    http: std::sync::Arc<serenity::http::Http>,
    channel_id: serenity::all::ChannelId,
    mut target: ProgressTarget,
    mut progress_rx: watch::Receiver<ResponseProgress>,
) {
    let public = target.is_public();
    let mut pending_edit = false;
    let mut last_edit = None;
    let mut edit_tick = interval(Duration::from_millis(550));
    edit_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    edit_tick.tick().await;

    let typing = async {
        let mut typing_tick = interval(Duration::from_secs(5));
        typing_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            typing_tick.tick().await;
            let _ = timeout(RESPONSE_STATUS_TIMEOUT, channel_id.broadcast_typing(&http)).await;
        }
    };
    tokio::pin!(typing);

    loop {
        tokio::select! {
            biased;
            changed = progress_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let progress = progress_rx.borrow_and_update().clone();
                match progress {
                    ResponseProgress::Completed => {
                        if public {
                            delete_progress_target(&http, &mut target).await;
                        }
                        break;
                    }
                    ResponseProgress::Failed(content) => {
                        edit_progress_target(&http, &mut target, content).await;
                        break;
                    }
                    ResponseProgress::Canceled => {
                        delete_progress_target(&http, &mut target).await;
                        break;
                    }
                    progress => {
                        if let Some(content) = render_progress(&progress) {
                            if last_edit.map_or(true, |last: Instant| {
                                last.elapsed() >= Duration::from_millis(550)
                            }) {
                                edit_progress_target(&http, &mut target, content).await;
                                last_edit = Some(Instant::now());
                                edit_tick.reset_after(Duration::from_millis(550));
                                pending_edit = false;
                            } else {
                                pending_edit = true;
                            }
                        } else {
                            pending_edit = false;
                        }
                    }
                }
            }
            _ = edit_tick.tick(), if pending_edit => {
                let progress = progress_rx.borrow_and_update().clone();
                if let Some(content) = render_progress(&progress) {
                    edit_progress_target(&http, &mut target, content).await;
                    last_edit = Some(Instant::now());
                    edit_tick.reset_after(Duration::from_millis(550));
                }
                pending_edit = false;
            }
            _ = &mut typing, if public => {}
        }
    }
}

async fn wait_for_cancel(cancel_rx: &mut watch::Receiver<bool>) {
    loop {
        if *cancel_rx.borrow_and_update() {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn finish_progress(
    progress_sender: &watch::Sender<ResponseProgress>,
    controller: JoinHandle<()>,
    terminal: ResponseProgress,
) {
    progress_sender.send_replace(terminal);
    if let Err(error) = controller.await {
        warn!("response progress controller failed: {error}");
    }
}

fn response_failure_content(origin: &ResponseOrigin, reason: &str) -> String {
    let fallback = match reason {
        "reasoning error" => "-# Error during reasoning".to_string(),
        "timeout" => "-# Error timeout".to_string(),
        "delivery error" => "-# Error during delivery".to_string(),
        "delivery timeout" => "-# Error delivery timeout".to_string(),
        _ => format!("-# Error {reason}"),
    };
    origin.failure_message(reason).unwrap_or(fallback)
}

async fn deliver_response(
    http: &serenity::http::Http,
    origin: &ResponseOrigin,
    channel_id: serenity::all::ChannelId,
    text: &str,
    footer: &str,
    sent_via_discord_tool: Option<DiscordSendRecord>,
    generated_images: Vec<Vec<u8>>,
) -> Result<(), serenity::Error> {
    if let Some(modal) = origin.modal_ephemeral_delivery() {
        if let Some(sent) = sent_via_discord_tool {
            if sent.private {
                debug!(
                    "Skipping final assistant send because discord-tool already sent an ephemeral interaction response"
                );
                send_modal_ephemeral_response(http, &modal.interaction, &sent.content, footer)
                    .await?;
                send_modal_generated_images(http, &modal.interaction, generated_images).await?;
                return Ok(());
            }

            debug!(
                "Skipping final assistant send because discord-tool already posted a public modal response"
            );
            append_discord_tool_footer(http, channel_id, &sent, footer).await?;
            modal
                .interaction
                .edit_response(
                    http,
                    EditInteractionResponse::new().content(MODAL_PUBLIC_RESPONSE_MESSAGE),
                )
                .await
                .ok();
            send_generated_images(http, channel_id, generated_images).await?;
            return Ok(());
        }

        send_modal_ephemeral_response(http, &modal.interaction, text, footer).await?;
        send_modal_generated_images(http, &modal.interaction, generated_images).await?;
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

        if is_same_text_loosely(&sent.content, text) {
            debug!(
                "Skipping final assistant send because discord-tool already posted the same text"
            );
            append_discord_tool_footer(http, channel_id, &sent, footer).await?;
            send_generated_images(http, channel_id, generated_images).await?;
            return Ok(());
        }
    }

    send_long_message(http, channel_id, text, footer).await?;
    send_generated_images(http, channel_id, generated_images).await?;
    Ok(())
}

async fn append_discord_tool_footer(
    http: &serenity::http::Http,
    fallback_channel_id: serenity::all::ChannelId,
    sent: &DiscordSendRecord,
    footer: &str,
) -> Result<(), serenity::Error> {
    if let (Some(channel_id), Some(message_id)) = (sent.channel_id, sent.message_id) {
        append_footer_to_sent_message(
            http,
            serenity::all::ChannelId::new(channel_id),
            serenity::all::MessageId::new(message_id),
            &sent.content,
            footer,
        )
        .await
    } else {
        fallback_channel_id
            .send_message(http, CreateMessage::new().content(footer))
            .await?;
        Ok(())
    }
}

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
    let (cancel_sender, cancel_receiver) = watch::channel(false);

    let task_handle = tokio::spawn(async move {
        if let Err(e) = run_response_task(
            &ctx_cloned,
            &ob_ctx_cloned,
            origin_cloned,
            request_id,
            cancel_receiver,
        )
        .await
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
        cancel_sender,
    };

    if let Some(old_active) = ob_context.active_responses.insert(channel_id, new_active) {
        let _ = old_active.cancel_sender.send(true);
    }
}

async fn run_response_task(
    ctx: &serenity::client::Context,
    ob_context: &NelfieContext,
    origin: ResponseOrigin,
    request_id: u64,
    mut cancel_rx: watch::Receiver<bool>,
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
            match timeout(
                RESPONSE_STATUS_TIMEOUT,
                channel_id.send_message(&ctx.http, CreateMessage::new().content(message)),
            )
            .await
            {
                Ok(result) => {
                    result?;
                }
                Err(_) => warn!(
                    "timed out sending rate limit notice: request_id={} channel={}",
                    request_id,
                    channel_id.get()
                ),
            }
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

    let timeout_duration = Duration::from_millis(ob_context.config.timeout_millis);
    let response_deadline = TokioInstant::now() + timeout_duration;
    let request_context = origin.current_request_context();
    let mut context = ob_context.chat_contexts.get_or_create(channel_id);
    if let Some(request_context) = request_context.as_ref() {
        context.extend(request_context);
    }
    if origin.triggers_context_compaction() {
        schedule_context_compaction(ob_context, channel_id);
    }

    let tools = ob_context.tools.clone();
    let target = if let Some(modal) = origin.modal_ephemeral_delivery() {
        ProgressTarget::Modal(modal.interaction.clone())
    } else {
        ProgressTarget::Public {
            channel_id,
            message_id: None,
        }
    };

    if *cancel_rx.borrow() {
        let mut target = target;
        delete_progress_target(&ctx.http, &mut target).await;
        return Ok(());
    }

    let (progress_sender, progress_rx) = watch::channel(ResponseProgress::Preparing);
    let progress_controller = tokio::spawn(run_progress_controller(
        ctx.http.clone(),
        channel_id,
        target,
        progress_rx,
    ));

    let preparation = tokio::select! {
        biased;
        _ = wait_for_cancel(&mut cancel_rx) => {
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Canceled,
            ).await;
            return Ok(());
        }
        result = timeout_at(response_deadline, async {
        let channel_name = channel_id
            .name(&ctx.http)
            .await
            .unwrap_or_else(|_| "None".to_string());

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

        context.add_text(system_prompt, Role::System);
        let request_token_limit = compaction_target_tokens(
            ob_context.config.context_compaction_token_limit,
        );
        let (dropped_items, estimated_tokens) =
            context.trim_to_recent_token_limit(request_token_limit);
        if dropped_items > 0 {
            info!(
                "bounded response context: request_id={} channel={} dropped_items={} estimated_tokens={} limit={}",
                request_id,
                channel_id.get(),
                dropped_items,
                estimated_tokens,
                request_token_limit
            );
        }
        refresh_discord_image_urls_for_api(ctx, &mut context).await;
        Ok::<(), serenity::Error>(())
        }) => result,
    };
    match preparation {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "reasoning error")),
            )
            .await;
            return Err(Box::new(error));
        }
        Err(_) => {
            warn!(
                "response timed out during preparation: request_id={} source={} channel={} timeout_ms={}",
                request_id,
                source,
                channel_id.get(),
                ob_context.config.timeout_millis
            );
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "timeout")),
            )
            .await;
            return Ok(());
        }
    }

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

    let context_cache_key = format!("nelfie-context-{}", channel_id.get());
    let generate_response = ob_context.lm_client.generate_response(
        ob_context.clone(),
        &context,
        GenerateResponseOptions {
            channel_id,
            typst_drafts: std::sync::Arc::new(crate::typst_render::TypstDraftStore::default()),
            max_output_tokens: MAX_RESPONSE_OUTPUT_TOKENS,
            tools,
            progress_sender: progress_sender.clone(),
            context_cache_key,
            parameters: model.to_parameter(),
            user_context_scope,
            allow_mainline_model_change: matches!(&origin, ResponseOrigin::User(_)),
            rate_usage_sender: rate_usage_tx,
        },
    );
    let generation_result = {
        let generation = async {
            if let Some(ephemeral_context) = origin.ephemeral_interaction_context() {
                scope_ephemeral_interaction(ephemeral_context, generate_response).await
            } else {
                generate_response.await
            }
        };
        tokio::pin!(generation);
        tokio::select! {
            biased;
            _ = wait_for_cancel(&mut cancel_rx) => None,
            result = timeout_at(response_deadline, &mut generation) => Some(result),
        }
    };

    let result = match generation_result {
        None => {
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Canceled,
            )
            .await;
            let _ = rate_usage_task.await;
            return Ok(());
        }
        Some(Ok(Ok(result))) => result,
        Some(Ok(Err(error))) => {
            let _ = rate_usage_task.await;
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "reasoning error")),
            )
            .await;
            return Err(error);
        }
        Some(Err(_)) => {
            let _ = rate_usage_task.await;
            warn!(
                "response timed out: request_id={} source={} channel={} timeout_ms={}",
                request_id,
                source,
                channel_id.get(),
                ob_context.config.timeout_millis
            );
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "timeout")),
            )
            .await;
            return Ok(());
        }
    };

    let _ = rate_usage_task.await;

    let generated_images = result.generated_images;
    let final_model = result.final_model;
    let metrics = result.metrics;
    let result_context = result.context;

    ob_context.chat_contexts.marge(channel_id, &result_context);

    let elapsed = start.elapsed().as_millis();
    let text = result_context.get_result();
    let sent_via_discord_tool = result_context.get_latest_discord_send_record();
    let total_tokens = metrics
        .total_tokens()
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let (cache_status, cached_tokens, input_tokens) = metrics
        .cached_tokens()
        .zip(metrics.input_tokens())
        .map(|(cached_tokens, input_tokens)| {
            (
                if cached_tokens > 0 { "hit" } else { "miss" },
                cached_tokens.to_string(),
                input_tokens.to_string(),
            )
        })
        .unwrap_or_else(|| ("unknown", "unknown".to_string(), "unknown".to_string()));
    let output_tokens = metrics.output_tokens();
    let output_tokens_log = output_tokens
        .map(|tokens| tokens.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    info!(
        "response generated: request_id={} source={} channel={} model={} elapsed_ms={} total_tokens={} prompt_cache={} cached_tokens={}/{} output_tokens={} output_chars={}",
        request_id,
        source,
        channel_id.get(),
        final_model,
        elapsed,
        total_tokens,
        cache_status,
        cached_tokens,
        input_tokens,
        output_tokens_log,
        text.chars().count()
    );
    info!("response output: request_id={} text={}", request_id, text);

    let output_footer = output_tokens
        .map(|tokens| format!(", output tokens: {tokens}"))
        .unwrap_or_default();
    let footer = format!(
        "-# Reasoning done in {}ms, model: {}{}",
        elapsed, final_model, output_footer
    );
    let delivery_result = {
        let delivery = deliver_response(
            &ctx.http,
            &origin,
            channel_id,
            &text,
            &footer,
            sent_via_discord_tool,
            generated_images,
        );
        tokio::pin!(delivery);
        tokio::select! {
            biased;
            _ = wait_for_cancel(&mut cancel_rx) => None,
            result = timeout(RESPONSE_DELIVERY_TIMEOUT, &mut delivery) => Some(result),
        }
    };

    match delivery_result {
        None => {
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Canceled,
            )
            .await;
            Ok(())
        }
        Some(Ok(Ok(()))) => {
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Completed,
            )
            .await;
            Ok(())
        }
        Some(Ok(Err(error))) => {
            warn!(
                "response delivery failed: request_id={} source={} channel={} error={}",
                request_id,
                source,
                channel_id.get(),
                error
            );
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "delivery error")),
            )
            .await;
            Err(Box::new(error))
        }
        Some(Err(_)) => {
            warn!(
                "response delivery timed out: request_id={} source={} channel={} timeout_ms={}",
                request_id,
                source,
                channel_id.get(),
                RESPONSE_DELIVERY_TIMEOUT.as_millis()
            );
            finish_progress(
                &progress_sender,
                progress_controller,
                ResponseProgress::Failed(response_failure_content(&origin, "delivery timeout")),
            )
            .await;
            Ok(())
        }
    }
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

#[cfg(test)]
mod tests {
    use tokio::sync::watch;

    use super::{render_progress, strip_status_prefix, truncate_progress_content, wait_for_cancel};
    use crate::llm::client::{ResponseProgress, ToolProgress};

    #[test]
    fn progress_states_render_with_one_simple_prefix() {
        assert_eq!(render_progress(&ResponseProgress::Preparing), None);
        assert_eq!(
            render_progress(&ResponseProgress::ToolRunning {
                tools: vec![
                    ToolProgress {
                        label: "-# -# web search".to_string(),
                        explanation: "-# -# searching the web...".to_string(),
                    },
                    ToolProgress {
                        label: "discord".to_string(),
                        explanation: "sending a Discord message".to_string(),
                    },
                ],
            })
            .as_deref(),
            Some("-# web search: searching the web...\n-# discord: sending a Discord message")
        );
        assert_eq!(render_progress(&ResponseProgress::Canceled), None);
        assert_eq!(render_progress(&ResponseProgress::Completed), None);
    }

    #[test]
    fn existing_status_prefix_is_not_added_twice() {
        assert_eq!(strip_status_prefix("  -# -# web search  "), "web search");
        assert_eq!(
            render_progress(&ResponseProgress::Failed("-# Error timeout".to_string())).as_deref(),
            Some("-# Error timeout")
        );
    }

    #[test]
    fn progress_truncation_respects_utf16_limit() {
        let truncated = truncate_progress_content("ab😀tail", 4);
        assert_eq!(truncated, "ab…");
        assert!(truncated.encode_utf16().count() <= 4);
    }

    #[tokio::test]
    async fn progress_watch_keeps_only_the_latest_state() {
        let (sender, mut receiver) = watch::channel(ResponseProgress::Preparing);
        sender.send_replace(ResponseProgress::ToolRunning {
            tools: vec![ToolProgress {
                label: "web search".to_string(),
                explanation: "searching the web...".to_string(),
            }],
        });

        receiver.changed().await.unwrap();
        assert_eq!(
            *receiver.borrow_and_update(),
            ResponseProgress::ToolRunning {
                tools: vec![ToolProgress {
                    label: "web search".to_string(),
                    explanation: "searching the web...".to_string(),
                }],
            }
        );
    }

    #[tokio::test]
    async fn cancel_watch_releases_the_waiter() {
        let (sender, receiver) = watch::channel(false);
        let mut receiver = receiver;
        sender.send_replace(true);

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            wait_for_cancel(&mut receiver),
        )
        .await
        .expect("cancel waiter should observe the latest value");
    }
}

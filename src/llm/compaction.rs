use log::{info, warn};
use serenity::all::ChannelId;
use tokio::time::{Duration, timeout};

use crate::app::context::NelfieContext;

pub fn schedule_context_compaction(ob_context: &NelfieContext, channel_id: ChannelId) {
    let Some((compaction_id, compaction)) = ob_context
        .chat_contexts
        .begin_compaction(channel_id, ob_context.config.context_compaction_token_limit)
    else {
        return;
    };

    let task_context = ob_context.clone();
    tokio::spawn(async move {
        let source_item_count = compaction.source_item_count();
        let summary = timeout(
            Duration::from_millis(task_context.config.timeout_millis),
            task_context.lm_client.summarize_context(
                compaction.summary_context(),
                task_context.config.context_summary_max_output_tokens,
            ),
        )
        .await;

        match summary {
            Ok(Ok(summary)) => {
                if task_context.chat_contexts.complete_compaction(
                    channel_id,
                    compaction_id,
                    &compaction,
                    &summary,
                ) {
                    info!(
                        "context compacted: channel={} source_items={} summary_chars={} model=gpt-5.6-luna",
                        channel_id.get(),
                        source_item_count,
                        summary.chars().count()
                    );
                } else {
                    warn!(
                        "context summary discarded: channel={} reason=source_changed",
                        channel_id.get()
                    );
                }
            }
            Ok(Err(error)) => {
                task_context
                    .chat_contexts
                    .cancel_compaction(channel_id, compaction_id);
                warn!(
                    "context summarization failed: channel={} error={}",
                    channel_id.get(),
                    error
                );
            }
            Err(_) => {
                task_context
                    .chat_contexts
                    .cancel_compaction(channel_id, compaction_id);
                warn!(
                    "context summarization timed out: channel={}",
                    channel_id.get()
                );
            }
        }
    });
}

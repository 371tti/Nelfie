use std::io;

use log::{info, warn};
use serenity::all::ChannelId;
use tokio::time::{Duration, Instant, timeout};

use crate::app::context::NelfieContext;
use crate::llm::{
    context::{
        LMContext, LMContextCompaction, Role, SUMMARY_INPUT_MAX_BYTES, compaction_target_tokens,
        split_utf8_chunks,
    },
    models::Models,
};

const SUMMARY_REDUCTION_MAX_OUTPUT_TOKENS: u32 = 3_072;

enum CompactionRun {
    Complete {
        batches: usize,
        source_items: usize,
        oversized_turns: usize,
        no_progress: bool,
    },
    Discarded,
}

pub fn schedule_context_compaction(ob_context: &NelfieContext, channel_id: ChannelId) {
    let trigger_token_limit = ob_context.config.context_compaction_token_limit;
    let target_token_limit = compaction_target_tokens(trigger_token_limit);
    let task_context = ob_context.clone();
    tokio::spawn(async move {
        let summary_timeout = Duration::from_millis(task_context.config.timeout_millis);
        let compaction_started = Instant::now();
        let Some((compaction_id, first_compaction)) = task_context.chat_contexts.begin_compaction(
            channel_id,
            trigger_token_limit,
            target_token_limit,
        ) else {
            return;
        };

        let remaining_timeout = summary_timeout.saturating_sub(compaction_started.elapsed());
        let run = timeout(remaining_timeout, async {
            let mut next_compaction = Some(first_compaction);
            let mut batches = 0usize;
            let mut source_items = 0usize;
            let mut oversized_turns = 0usize;

            while let Some(compaction) = next_compaction.take() {
                let batch_source_items = compaction.source_item_count();
                let summary = summarize_compaction(
                    &task_context,
                    &compaction,
                    task_context.config.context_summary_max_output_tokens,
                )
                .await?;
                let estimated_savings = compaction.estimated_token_savings(&summary);
                if estimated_savings == 0 {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                        CompactionRun::Complete {
                            batches,
                            source_items,
                            oversized_turns,
                            no_progress: true,
                        },
                    );
                }

                if !task_context.chat_contexts.complete_compaction(
                    channel_id,
                    compaction_id,
                    &compaction,
                    &summary,
                ) {
                    return Ok(CompactionRun::Discarded);
                }

                batches += 1;
                source_items += batch_source_items;
                oversized_turns += usize::from(compaction.oversized_first_turn());
                next_compaction = task_context.chat_contexts.continue_compaction(
                    channel_id,
                    compaction_id,
                    target_token_limit,
                );
            }

            Ok(CompactionRun::Complete {
                batches,
                source_items,
                oversized_turns,
                no_progress: false,
            })
        })
        .await;

        match run {
            Ok(Ok(CompactionRun::Complete {
                batches,
                source_items,
                oversized_turns,
                no_progress,
            })) => {
                let recheck = task_context
                    .chat_contexts
                    .finish_compaction(channel_id, compaction_id);
                if no_progress {
                    warn!(
                        "context compaction stopped: channel={} reason=no_estimated_savings",
                        channel_id.get()
                    );
                } else if batches > 0 {
                    info!(
                        "context compacted: channel={} batches={} source_items={} oversized_turns={} model={}",
                        channel_id.get(),
                        batches,
                        source_items,
                        oversized_turns,
                        Models::CONTEXT_SUMMARY_MODEL
                    );
                }
                if recheck {
                    schedule_context_compaction(&task_context, channel_id);
                }
            }
            Ok(Ok(CompactionRun::Discarded)) => {
                warn!(
                    "context summary discarded: channel={} reason=source_changed",
                    channel_id.get()
                );
                if task_context
                    .chat_contexts
                    .cancel_compaction(channel_id, compaction_id)
                {
                    schedule_context_compaction(&task_context, channel_id);
                }
            }
            Ok(Err(error)) => {
                warn!(
                    "context summarization failed: channel={} error={}",
                    channel_id.get(),
                    error
                );
                if task_context
                    .chat_contexts
                    .cancel_compaction(channel_id, compaction_id)
                {
                    schedule_context_compaction(&task_context, channel_id);
                }
            }
            Err(_) => {
                warn!(
                    "context summarization timed out: channel={}",
                    channel_id.get()
                );
                if task_context
                    .chat_contexts
                    .cancel_compaction(channel_id, compaction_id)
                {
                    schedule_context_compaction(&task_context, channel_id);
                }
            }
        }
    });
}

async fn summarize_compaction(
    context: &NelfieContext,
    compaction: &LMContextCompaction,
    max_output_tokens: u32,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut summaries = Vec::with_capacity(compaction.summary_contexts().len());
    for summary_context in compaction.summary_contexts() {
        let summary = context
            .lm_client
            .summarize_context(summary_context, max_output_tokens)
            .await?;
        summaries.push(summary.text);
    }

    while summaries.len() > 1 {
        summaries = split_oversized_summaries(summaries);
        let groups = group_summaries(&summaries);
        if groups.len() == summaries.len() {
            let original_bytes = summaries
                .iter()
                .map(|summary| {
                    summary_context_for_texts(std::slice::from_ref(summary))
                        .serialized_input_bytes()
                })
                .sum::<usize>();
            let mut reduced = Vec::with_capacity(summaries.len());
            for summary in &summaries {
                let result = context
                    .lm_client
                    .summarize_context(
                        &summary_context_for_texts(std::slice::from_ref(summary)),
                        max_output_tokens
                            .min(SUMMARY_REDUCTION_MAX_OUTPUT_TOKENS)
                            .max(1),
                    )
                    .await?;
                reduced.push(result.text);
            }
            let reduced_bytes = reduced
                .iter()
                .map(|summary| {
                    summary_context_for_texts(std::slice::from_ref(summary))
                        .serialized_input_bytes()
                })
                .sum::<usize>();
            if reduced_bytes >= original_bytes {
                return Err(Box::new(io::Error::other(
                    "summary segments cannot be reduced within the input byte limit",
                )));
            }
            summaries = reduced;
            continue;
        }

        let mut merged = Vec::with_capacity(groups.len());
        for group in groups {
            if group.len() == 1 {
                merged.push(group.into_iter().next().unwrap_or_default());
                continue;
            }
            let summary_context = summary_context_for_texts(&group);
            let result = context
                .lm_client
                .summarize_context(&summary_context, max_output_tokens)
                .await?;
            merged.push(result.text);
        }
        if merged.len() >= summaries.len() {
            return Err(Box::new(io::Error::other(
                "summary reduction made no progress",
            )));
        }
        summaries = merged;
    }

    Ok(summaries.pop().unwrap_or_default())
}

fn summary_context_for_texts(texts: &[String]) -> LMContext {
    let mut context = LMContext::new();
    for (index, text) in texts.iter().enumerate() {
        context.add_text(
            format!("Earlier-to-later summary segment {}:\n{text}", index + 1),
            Role::User,
        );
    }
    context
}

fn group_summaries(summaries: &[String]) -> Vec<Vec<String>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();

    for summary in summaries {
        let mut candidate = current.clone();
        candidate.push(summary.clone());
        let candidate_context = summary_context_for_texts(&candidate);
        if candidate_context.serialized_input_bytes() <= SUMMARY_INPUT_MAX_BYTES {
            current = candidate;
        } else {
            if !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
            current.push(summary.clone());
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

fn split_oversized_summaries(summaries: Vec<String>) -> Vec<String> {
    let mut bounded = Vec::new();

    for summary in summaries {
        if summary_context_for_texts(std::slice::from_ref(&summary)).serialized_input_bytes()
            <= SUMMARY_INPUT_MAX_BYTES
        {
            bounded.push(summary);
            continue;
        }

        let mut chunk_bytes = SUMMARY_INPUT_MAX_BYTES / 4;
        loop {
            let chunks = split_utf8_chunks(&summary, chunk_bytes)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if chunks.iter().all(|chunk| {
                summary_context_for_texts(std::slice::from_ref(chunk)).serialized_input_bytes()
                    <= SUMMARY_INPUT_MAX_BYTES
            }) || chunk_bytes == 1
            {
                bounded.extend(chunks);
                break;
            }
            chunk_bytes = (chunk_bytes / 2).max(1);
        }
    }
    bounded
}
